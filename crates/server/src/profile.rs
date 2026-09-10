//! Profile identity: rendering local accounts as actor documents and
//! applying profile changes (`update_credentials`), including the
//! `Update(Actor)` fan-out that tells followers about them.

use plamenu_ap::actor::{
    Actor, GroupActorParams, Image, LocalActorParams, Multikey, PropertyValue, PublicKey,
};
use plamenu_ap::urls::LocalUserUrls;
use plamenu_db::account::{self, Account, ProfileUpdate};
use plamenu_db::id;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;

use crate::AppState;
use crate::compose::compose;
use crate::entities::field_value_html;
use crate::error::ApiError;
use crate::media_processing::{
    AVATAR_MAX_EDGE, HEADER_MAX_EDGE, content_type_for, process_image_blocking,
};

/// Mastodon's profile limits. Also advertised in `configuration.accounts`
/// (`routes::api`), so a client sizes its profile editor to what we validate.
pub(crate) const MAX_DISPLAY_NAME_CHARS: usize = 30;
pub(crate) const MAX_NOTE_CHARS: usize = 500;
pub(crate) const MAX_FIELDS: usize = 4;
pub(crate) const MAX_FIELD_CHARS: usize = 255;

struct PublishedActorKeys {
    rsa_id: String,
    rsa_public: String,
    methods: Vec<Multikey>,
}

async fn published_actor_keys(
    state: &AppState,
    account: &Account,
) -> Result<PublishedActorKeys, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    published_actor_keys_conn(state, &mut conn, account).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
async fn published_actor_keys_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    account: &Account,
) -> Result<PublishedActorKeys, ApiError> {
    let urls = LocalUserUrls::for_account(
        &state.config.domain,
        &account.username,
        account.uri.as_deref(),
    );
    let actor_id = urls.id.clone();
    let usable = plamenu_db::actor_key::published_for_account(&mut *conn, account.id).await?;
    let rsa = usable.iter().find(|key| {
        key.algorithm == "rsa" && key.controller_uri == actor_id && !key.public_key.is_empty()
    });
    if let Some(rsa) = rsa {
        let mut methods = Vec::with_capacity(usable.len());
        // Ed25519 first is a compatibility hint only. Exact key IDs remain
        // authoritative and every usable overlap key is published.
        for algorithm in ["ed25519", "rsa", "ml-dsa-44"] {
            for key in usable
                .iter()
                .filter(|key| key.algorithm == algorithm && key.controller_uri == actor_id)
            {
                let method = match algorithm {
                    "ed25519" | "ml-dsa-44" => Some(Multikey {
                        id: key.key_uri.clone(),
                        kind: "Multikey".to_owned(),
                        controller: actor_id.clone(),
                        public_key_multibase: key.public_key.clone(),
                    }),
                    "rsa" => Multikey::rsa(key.key_uri.clone(), actor_id.clone(), &key.public_key),
                    _ => None,
                };
                methods.extend(method);
            }
        }
        return Ok(PublishedActorKeys {
            rsa_id: rsa.key_uri.clone(),
            rsa_public: rsa.public_key.clone(),
            methods,
        });
    }

    // Fixture/rollback-window compatibility. Production startup backfills
    // normalized encrypted rows before requests are served.
    let mut methods = Vec::new();
    if let Some(ed) = account::ed25519_public_key(&mut *conn, account.id).await? {
        methods.push(Multikey::ed25519(
            urls.ed25519_key_id,
            actor_id.clone(),
            &ed,
        ));
    }
    methods.extend(Multikey::rsa(
        urls.key_id.clone(),
        actor_id.clone(),
        &account.public_key,
    ));
    Ok(PublishedActorKeys {
        rsa_id: urls.key_id,
        rsa_public: account.public_key.clone(),
        methods,
    })
}

fn apply_published_keys(mut actor: Actor, keys: PublishedActorKeys) -> Actor {
    actor.public_key = PublicKey {
        id: keys.rsa_id,
        owner: actor.id.clone(),
        public_key_pem: keys.rsa_public,
    };
    actor.assertion_method = keys.methods;
    actor
}

/// Renders a local account as its actor document (also the object of
/// outgoing `Update(Actor)` activities).
pub async fn local_actor(state: &AppState, account: &Account) -> Result<Actor, ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    local_actor_conn(state, &mut conn, account).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn local_actor_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    account: &Account,
) -> Result<Actor, ApiError> {
    let domain = state.config.domain.as_str();
    let published = account
        .created_at
        .format(&Rfc3339)
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    if account.suspended() {
        return suspended_actor_conn(state, conn, account, published).await;
    }
    let image_of = |file_name: &Option<String>, description: &str| {
        file_name.as_ref().map(|file| {
            Image::new(
                format!("https://{domain}/media/{file}"),
                content_type_for(file),
            )
            .with_summary(description)
        })
    };
    let fields = account
        .fields
        .as_array()
        .map(|stored| {
            stored
                .iter()
                .map(|field| {
                    let name = field.get("name").and_then(Value::as_str).unwrap_or("");
                    let value = field.get("value").and_then(Value::as_str).unwrap_or("");
                    PropertyValue::new(name.to_owned(), field_value_html(account, value))
                })
                .collect()
        })
        .unwrap_or_default();
    // Custom emoji referenced anywhere in the profile text federate as
    // `Emoji` tag entries, like Mastodon's `emojifiable_text` — and each
    // featured tag rides along as a `Hashtag` object (Mastodon's
    // `virtual_tags` emits both kinds in the actor `tag` array).
    let emojifiable = crate::entities::account_emojifiable_text(account);
    let mut emoji_tags = crate::emoji::emoji_tags_for_text_for_account(
        &mut *conn,
        domain,
        account.id,
        &[&emojifiable],
    )
    .await?;
    for featured in plamenu_db::featured_tag::list(&mut *conn, account.id).await? {
        emoji_tags.push(plamenu_ap::actor::profile_hashtag_tag(
            domain,
            &account.username,
            &featured.name,
        ));
    }
    let keys = published_actor_keys_conn(state, conn, account).await?;
    // A local group's community flags ride the actor document. The
    // sidecar row is created with the account, so a Group without one only
    // means a half-created row — fall back to the defaults.
    let group = if account.is_group() {
        let sidecar = plamenu_db::group::find(&mut *conn, account.id).await?;
        Some(GroupActorParams {
            posting_restricted_to_mods: sidecar
                .as_ref()
                .is_some_and(plamenu_db::group::Group::posting_restricted_to_mods),
            sensitive: sidecar.as_ref().is_some_and(|g| g.sensitive),
            // The full policy under our own term: `Members` is invisible in
            // Lemmy's boolean, so without this even another Plamenu would
            // read a members-only community as open.
            posting_policy: sidecar
                .as_ref()
                .map_or(plamenu_db::group::PostingPolicy::default(), |g| {
                    g.posting_policy()
                })
                .as_str(),
        })
    } else {
        None
    };
    let mut actor = apply_published_keys(
        Actor::local_person(&LocalActorParams {
            domain,
            username: &account.username,
            actor_id: account.uri.as_deref(),
            display_name: (!account.display_name.is_empty())
                .then_some(account.display_name.as_str()),
            summary: (!account.note.is_empty()).then_some(account.note.as_str()),
            public_key_pem: &keys.rsa_public,
            published,
            icon: image_of(&account.avatar_file_name, &account.avatar_description),
            image: image_of(&account.header_file_name, &account.header_description),
            fields,
            emoji_tags,
            locked: account.locked,
            discoverable: account.discoverable.unwrap_or(false),
            // `hideCollections` is enforced locally (Mastodon doesn't federate it
            // in the actor), so only the bot/indexable flags reach the document.
            bot: account.is_bot,
            indexable: account.indexable,
            attribution_domains: account::attribution_domains(&mut *conn, account.id).await?,
            memorial: account.memorial,
            suspended: false,
            show_featured: account.show_featured,
            show_media: account.show_media,
            show_media_replies: account.show_media_replies,
            also_known_as: account.also_known_as.clone(),
            moved_to: account.moved_to_uri.clone(),
            ed25519_public_multibase: None,
            group,
        }),
        keys,
    );
    let proofs = crate::identity::list_conn(conn, account, &actor.id).await?;
    actor.attachment.extend(proofs);
    Ok(actor)
}

/// The blanked actor document of a temporarily suspended account, Mastodon's
/// `unavailable?` serialization: identity and routing fields (id, inbox,
/// key material, `published`) survive, the profile is stripped — bare
/// username for `name`, no bio/avatar/header/fields/tags/aliases — and
/// `suspended: true` marks the state so peers apply a remote-origin
/// suspension instead of treating the account as gone.
///
/// Connection-scoped delivery for an enclosing mutation transaction.
async fn suspended_actor_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    account: &Account,
    published: String,
) -> Result<Actor, ApiError> {
    let keys = published_actor_keys_conn(state, conn, account).await?;
    Ok(apply_published_keys(
        Actor::local_person(&LocalActorParams {
            domain: state.config.domain.as_str(),
            username: &account.username,
            actor_id: account.uri.as_deref(),
            display_name: Some(&account.username),
            summary: None,
            public_key_pem: &keys.rsa_public,
            published,
            icon: None,
            image: None,
            fields: Vec::new(),
            emoji_tags: Vec::new(),
            locked: false,
            discoverable: false,
            bot: account.is_bot,
            indexable: false,
            attribution_domains: Vec::new(),
            memorial: account.memorial,
            suspended: true,
            show_featured: account.show_featured,
            show_media: account.show_media,
            show_media_replies: account.show_media_replies,
            also_known_as: Vec::new(),
            moved_to: None,
            ed25519_public_multibase: None,
            group: None,
        }),
        keys,
    ))
}

/// A key-only "blanked" actor document, served to *unsigned* AP callers when
/// `authorized_fetch` (secure mode) is on. It carries exactly what a peer
/// needs to verify our outbound HTTP signatures and route a reply — id, actor
/// type, `preferredUsername`, inbox/outbox/endpoints and the public key(s) —
/// and withholds all profile PII (display name, bio, avatar/header, metadata
/// fields, aliases, discoverability). This lets peers that dereference us
/// unsigned (e.g. default-config Lemmy, whose `federation_signed_fetch`
/// defaults off) read our key to verify a delivery, while a valid signature is
/// still required to unlock the full profile.
///
/// Structurally the live-actor analogue of [`suspended_actor`]; kept a
/// separate function because the suspended document has its own federation
/// semantics (`suspended: true`, so peers mirror the suspension) that this
/// path must not emit. The real actor *type* (Person/Service/Group) is
/// preserved — it is not PII and mis-typing a group breaks interop — so a
/// local group keeps its community flags with the profile still blanked.
pub(crate) async fn key_only_actor(state: &AppState, account: &Account) -> Result<Actor, ApiError> {
    let domain = state.config.domain.as_str();
    let published = account
        .created_at
        .format(&Rfc3339)
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let keys = published_actor_keys(state, account).await?;
    let group = if account.is_group() {
        let sidecar = plamenu_db::group::find(&state.pool, account.id).await?;
        Some(GroupActorParams {
            posting_restricted_to_mods: sidecar
                .as_ref()
                .is_some_and(plamenu_db::group::Group::posting_restricted_to_mods),
            sensitive: sidecar.as_ref().is_some_and(|g| g.sensitive),
            // The full policy under our own term: `Members` is invisible in
            // Lemmy's boolean, so without this even another Plamenu would
            // read a members-only community as open.
            posting_policy: sidecar
                .as_ref()
                .map_or(plamenu_db::group::PostingPolicy::default(), |g| {
                    g.posting_policy()
                })
                .as_str(),
        })
    } else {
        None
    };
    Ok(apply_published_keys(
        Actor::local_person(&LocalActorParams {
            domain,
            username: &account.username,
            actor_id: account.uri.as_deref(),
            display_name: None,
            summary: None,
            public_key_pem: &keys.rsa_public,
            published,
            icon: None,
            image: None,
            fields: Vec::new(),
            emoji_tags: Vec::new(),
            // Protocol-relevant routing/follow semantics are kept; directory and
            // search exposure (the scraping surface secure mode protects) is not.
            locked: account.locked,
            discoverable: false,
            bot: account.is_bot,
            indexable: false,
            attribution_domains: Vec::new(),
            memorial: account.memorial,
            suspended: false,
            show_featured: account.show_featured,
            show_media: account.show_media,
            show_media_replies: account.show_media_replies,
            also_known_as: Vec::new(),
            moved_to: None,
            ed25519_public_multibase: None,
            group,
        }),
        keys,
    ))
}

/// A profile change request, already parsed out of whatever body format the
/// client used. `None` everywhere means "leave unchanged".
#[derive(Debug, Default)]
pub struct ProfileChanges {
    pub display_name: Option<String>,
    /// Raw bio text; rendered through the compose pipeline.
    pub note: Option<String>,
    /// Raw `(name, value)` pairs; an empty list clears the fields.
    pub fields: Option<Vec<(String, String)>>,
    /// Raw uploaded image bytes.
    pub avatar: Option<Vec<u8>>,
    pub header: Option<Vec<u8>>,
    /// Manually approve followers from now on. Pending requests are
    /// unaffected either way, like Mastodon.
    pub locked: Option<bool>,
    /// Discoverability opt-in (`toot:discoverable`).
    pub discoverable: Option<bool>,
    /// Automated account flag (`bot` / actor `type: Service`).
    pub bot: Option<bool>,
    /// Public-post search opt-in (`toot:indexable`).
    pub indexable: Option<bool>,
    /// Hide follows/followers collections (`hideCollections`).
    pub hide_collections: Option<bool>,
    /// Alt text for the avatar/header images (API v11).
    pub avatar_description: Option<String>,
    pub header_description: Option<String>,
    /// Profile-tab settings (`showMedia`/`showRepliesInMedia`/`showFeatured`);
    /// only the profile endpoint accepts them, like Mastodon.
    pub show_media: Option<bool>,
    pub show_media_replies: Option<bool>,
    pub show_featured: Option<bool>,
    /// Domains authorized for `fediverse:creator` attribution; an empty list
    /// clears them. Normalized before storage ([`normalize_attribution_domains`]).
    pub attribution_domains: Option<Vec<String>>,
}

/// Mastodon's editable attribution-domains cap.
const ATTRIBUTION_DOMAINS_LIMIT: usize = 100;

/// Mastodon's `normalizes :attribution_domains`: strip scheme and `*.`
/// prefixes, trim, drop blanks, dedupe (order-preserving).
#[must_use]
pub fn normalize_attribution_domains(raw: &[String]) -> Vec<String> {
    let mut seen = Vec::new();
    for entry in raw {
        let domain = entry
            .trim()
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .trim_start_matches("*.")
            .trim_end_matches('/')
            .to_lowercase();
        if !domain.is_empty() && !seen.contains(&domain) {
            seen.push(domain);
        }
    }
    seen
}

/// A plausible bare domain: dot-separated label chars, no scheme, path,
/// port or userinfo (Mastodon's `domain: true` validation).
fn is_valid_attribution_domain(domain: &str) -> bool {
    domain.len() <= 100
        && domain.contains('.')
        && domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        && !domain.starts_with(['.', '-'])
        && !domain.ends_with(['.', '-'])
}

fn validation_error(message: &str) -> ApiError {
    ApiError::Unprocessable(format!("Validation failed: {message}"))
}

fn validate(changes: &ProfileChanges) -> Result<(), ApiError> {
    if let Some(name) = &changes.display_name
        && name.chars().count() > MAX_DISPLAY_NAME_CHARS
    {
        return Err(validation_error(
            "Display name is too long (maximum is 30 characters)",
        ));
    }
    if let Some(note) = &changes.note
        && note.chars().count() > MAX_NOTE_CHARS
    {
        return Err(validation_error(
            "Note is too long (maximum is 500 characters)",
        ));
    }
    if let Some(fields) = &changes.fields {
        if fields.len() > MAX_FIELDS {
            return Err(validation_error("Too many profile fields"));
        }
        for (name, value) in fields {
            if name.chars().count() > MAX_FIELD_CHARS || value.chars().count() > MAX_FIELD_CHARS {
                return Err(validation_error(
                    "Fields are too long (maximum is 255 characters)",
                ));
            }
        }
    }
    if let Some(domains) = &changes.attribution_domains {
        if domains.len() > ATTRIBUTION_DOMAINS_LIMIT {
            return Err(validation_error(
                "Attribution domains are too long (maximum is 100 characters)",
            ));
        }
        if let Some(bad) = domains.iter().find(|d| !is_valid_attribution_domain(d)) {
            return Err(validation_error(&format!(
                "Attribution domains {bad} is not a valid domain name"
            )));
        }
    }
    Ok(())
}

/// Processes and stores an uploaded profile image, returning its file name and
/// stored byte size (for the admin `space_usage` metric). Shared with the group
/// console, whose groups are local accounts with the same avatar/header
/// columns — the processing rules must not diverge by who owns the profile.
pub(crate) async fn store_profile_image(
    state: &AppState,
    bytes: Vec<u8>,
    max_edge: u32,
) -> Result<(String, i64), ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    let params =
        crate::media_processing::EncodeParams::from_settings(&settings, &state.config.ffmpeg_path);
    let processed = process_image_blocking(bytes, max_edge, params).await?;
    let file_name = format!("{}.{}", id::next(), processed.extension);
    let file_size = i64::try_from(processed.bytes.len()).unwrap_or(i64::MAX);
    state
        .media
        .put(&file_name, processed.bytes)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    Ok((file_name, file_size))
}

/// Applies a profile change to a local account and fans the new actor
/// document out to follower inboxes as `Update(Actor)`.
pub async fn update_profile(
    state: &AppState,
    account: &Account,
    changes: ProfileChanges,
) -> Result<Account, ApiError> {
    validate(&changes)?;

    let composed = match &changes.note {
        Some(text) => Some(compose(state, text, crate::compose::PostFormat::Plain).await?),
        None => None,
    };
    // An existing rel="me" verification carries over while a field's value is
    // unchanged (Mastodon's `fields_attributes=`, applied by the storage
    // layer); editing the URL drops the badge until the worker re-checks.
    let fields_value = changes.fields.as_ref().map(|fields| {
        fields
            .iter()
            .filter(|(name, value)| !(name.trim().is_empty() && value.trim().is_empty()))
            .map(|(name, value)| account::FieldPair {
                name: name.trim().to_owned(),
                value: value.trim().to_owned(),
            })
            .collect()
    });
    let avatar_file = match changes.avatar {
        Some(bytes) => Some(store_profile_image(state, bytes, AVATAR_MAX_EDGE).await?),
        None => None,
    };
    let header_file = match changes.header {
        Some(bytes) => Some(store_profile_image(state, bytes, HEADER_MAX_EDGE).await?),
        None => None,
    };

    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let result: Result<Account, ApiError> = async {
        let outcome = account::update_local_profile_conn(
            &mut tx,
            account.id,
            ProfileUpdate {
                display_name: changes.display_name.as_deref(),
                note: composed.as_ref().map(|c| c.html.as_str()),
                note_source: changes.note.as_deref(),
                fields: fields_value,
                avatar_file_name: avatar_file.as_ref().map(|(name, _)| name.as_str()),
                header_file_name: header_file.as_ref().map(|(name, _)| name.as_str()),
                avatar_file_size: avatar_file.as_ref().map(|(_, size)| *size),
                header_file_size: header_file.as_ref().map(|(_, size)| *size),
                locked: changes.locked,
                discoverable: changes.discoverable,
                is_bot: changes.bot,
                indexable: changes.indexable,
                hide_collections: changes.hide_collections,
                avatar_description: changes.avatar_description.as_deref(),
                header_description: changes.header_description.as_deref(),
                show_media: changes.show_media,
                show_media_replies: changes.show_media_replies,
                show_featured: changes.show_featured,
            },
        )
        .await;
        let updated = outcome?.ok_or(ApiError::NotFound)?;
        if let Some(domains) = &changes.attribution_domains {
            account::set_attribution_domains(&mut *tx, updated.id, domains).await?;
        }
        fan_out_actor_update_conn(state, &mut tx, &updated).await?;
        // A field change may add or edit a URL to (re-)verify; the worker re-checks
        // every URL field, so one enqueue covers all of them.
        if changes.fields.is_some() {
            plamenu_db::link_verification::enqueue(&mut *tx, updated.id).await?;
        }
        Ok(updated)
    }
    .await;
    let updated = match result {
        Ok(updated) => updated,
        Err(error) => {
            drop(tx);
            enqueue_stray_profile_images(state, avatar_file.as_ref(), header_file.as_ref()).await;
            return Err(error);
        }
    };
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    // A replaced avatar/header supersedes the account's previous local file.
    // Unlike status attachments these images have no `media_attachments` row,
    // so `vacuum_orphans` can never rediscover the old key — and `/media/{file}`
    // serves any valid key without a DB check, so a leaked image stays publicly
    // fetchable. Remove the superseded file now.
    if avatar_file.is_some()
        && let Some(old) = account.avatar_file_name.as_deref()
        && updated.avatar_file_name.as_deref() != Some(old)
    {
        delete_superseded_profile_image(state, old).await;
    }
    if header_file.is_some()
        && let Some(old) = account.header_file_name.as_deref()
        && updated.header_file_name.as_deref() != Some(old)
    {
        delete_superseded_profile_image(state, old).await;
    }

    crate::webhooks::account_event(state, plamenu_db::webhook::ACCOUNT_UPDATED, updated.id).await;
    Ok(updated)
}

/// Which profile image a [`clear_profile_image`] call removes.
#[derive(Clone, Copy)]
pub enum ProfileImage {
    Avatar,
    Header,
}

/// Removes a local account's avatar or header (and its alt text), then fans
/// the updated actor out to followers — Mastodon's
/// `Profile::AvatarsController#destroy` / `HeadersController#destroy`.
pub async fn clear_profile_image(
    state: &AppState,
    account: &Account,
    which: ProfileImage,
) -> Result<Account, ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let updated = match which {
        ProfileImage::Avatar => account::clear_avatar(&mut *tx, account.id).await?,
        ProfileImage::Header => account::clear_header(&mut *tx, account.id).await?,
    }
    .ok_or(ApiError::NotFound)?;
    fan_out_actor_update_conn(state, &mut tx, &updated).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    // The cleared image's local file has no media row for the orphan sweep to
    // reclaim; remove it now so the bytes don't leak.
    let cleared = match which {
        ProfileImage::Avatar => account.avatar_file_name.as_deref(),
        ProfileImage::Header => account.header_file_name.as_deref(),
    };
    if let Some(old) = cleared {
        delete_superseded_profile_image(state, old).await;
    }
    crate::webhooks::account_event(state, plamenu_db::webhook::ACCOUNT_UPDATED, updated.id).await;
    Ok(updated)
}

/// Compensation for a profile update that stored image files and then failed
/// before committing their database reference: queue the stray files for
/// durable cleanup so they cannot leak.
pub(crate) async fn enqueue_stray_profile_images(
    state: &AppState,
    avatar: Option<&(String, i64)>,
    header: Option<&(String, i64)>,
) {
    let stray: Vec<String> = avatar
        .iter()
        .chain(header.iter())
        .map(|(name, _)| name.clone())
        .collect();
    if stray.is_empty() {
        return;
    }
    if let Err(error) = plamenu_db::media_cleanup::enqueue_many(&state.pool, &stray).await {
        tracing::error!(%error, "failed to enqueue stray profile images for cleanup");
    }
}

/// Queues a superseded local profile image (a replaced or cleared
/// avatar/header) for durable deletion. These files carry no
/// `media_attachments` row, so a missed delete leaks the bytes on disk and
/// leaves the old image publicly fetchable via `/media/{file}`. Deletion goes
/// through the leased `media_cleanup_jobs` queue (finding #39's worker) rather
/// than a best-effort direct store call, so a transient store error is retried
/// until the lease cap instead of silently leaking. The
/// reconciliation sweep remains the backstop for an enqueue lost to a crash
/// between the profile commit and this call.
pub(crate) async fn delete_superseded_profile_image(state: &AppState, file_name: &str) {
    if let Err(error) =
        plamenu_db::media_cleanup::enqueue_many(&state.pool, &[file_name.to_owned()]).await
    {
        tracing::error!(
            file = file_name,
            %error,
            "failed to enqueue superseded profile image for cleanup"
        );
    }
}

/// Fans the account's current actor document out to follower inboxes as
/// `Update(Actor)`.
pub async fn fan_out_actor_update(state: &AppState, account: &Account) -> Result<(), ApiError> {
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    fan_out_actor_update_conn(state, &mut conn, account).await
}

/// Connection-scoped delivery for an enclosing mutation transaction.
pub async fn fan_out_actor_update_conn(
    state: &AppState,
    conn: &mut plamenu_db::PgConnection,
    account: &Account,
) -> Result<(), ApiError> {
    let actor = local_actor_conn(state, conn, account).await?;
    let activity = plamenu_ap::activity::update_actor(
        &state.config.domain,
        &account.username,
        serde_json::to_value(actor).map_err(|e| ApiError::Internal(Box::new(e)))?,
        plamenu_db::id::next(),
    );
    let reach = plamenu_db::account::reach_inboxes(&mut *conn, account.id).await?;
    let delivered = crate::actions::fan_out_conn(state, conn, account, &activity, &reach).await?;
    tracing::info!(user = %account.username, inboxes = delivered, "profile updated");
    Ok(())
}
