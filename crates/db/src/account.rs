//! Account storage: local and remote actors.

use sqlx::{PgExecutor, PgPool};
use time::OffsetDateTime;

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors Mastodon's account flags"
)]
pub struct Account {
    pub id: i64,
    pub username: String,
    /// `None` for local accounts.
    pub domain: Option<String>,
    pub display_name: String,
    pub note: String,
    /// SPKI PEM compatibility copy. Normalized signing/verification material
    /// lives in `actor_keys`.
    pub public_key: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    /// Canonical `ActivityPub` actor id. Remote actors always provide it;
    /// production local actors persist either their legacy username URI or a
    /// handle-independent numeric URI. `None` is retained only for pre-backfill
    /// rows and legacy test fixtures.
    pub uri: Option<String>,
    pub inbox_url: String,
    pub shared_inbox_url: String,
    /// The keyId the remote actor publishes; `None` for local accounts.
    pub public_key_id: Option<String>,
    /// Local avatar/header uploads (served from `/media/{file}`).
    pub avatar_file_name: Option<String>,
    pub header_file_name: Option<String>,
    /// Remote actors' icon/image URLs (served by their origin).
    pub avatar_remote_url: Option<String>,
    pub header_remote_url: Option<String>,
    /// Alt text for the avatar/header images; empty when unset.
    pub avatar_description: String,
    pub header_description: String,
    /// Profile metadata fields: `[{"name": …, "value": …}]` with an optional
    /// `verified_at` per entry. Values are raw text for local accounts,
    /// sanitized HTML for remote ones. Stored as `account_fields` rows and
    /// assembled by the `account_fields_json()` SQL helper; write through
    /// [`ProfileUpdate::fields`] / [`RemoteAccountData::fields`].
    pub fields: serde_json::Value,
    /// The unrendered bio a local user typed; empty for remote accounts.
    pub note_source: String,
    /// Manually approves followers (`manuallyApprovesFollowers`).
    pub locked: bool,
    /// Other actors the same person controls (`alsoKnownAs`); a migration to
    /// this account is only honoured when its origin is listed here.
    pub also_known_as: Vec<String>,
    /// The actor this account migrated to (`movedTo`), once it has moved away.
    pub moved_to_uri: Option<String>,
    /// The human web URL (`url`), distinct from the AP id (`uri`): the remote
    /// origin's published `url` for remote accounts, `None` for local ones
    /// (derived as `/@username` by the serializers).
    pub url: Option<String>,
    /// Discoverability opt-in (`toot:discoverable`): listing in the profile
    /// directory/search and eligibility to be added to collections. `None`
    /// means never set, treated as `false` everywhere it is read.
    pub discoverable: Option<bool>,
    /// Remote actors' `interactionPolicy.canFeature` bitmap. Local accounts
    /// derive featureability from `discoverable`/`locked`, like Mastodon.
    pub feature_approval_policy: i32,
    /// Automated account: serialized as actor `type: Service` (`bot` in the
    /// Mastodon entity).
    pub is_bot: bool,
    /// Opt-in to public-post search (`toot:indexable`).
    pub indexable: bool,
    /// Hide the follows/followers collections (`hideCollections`).
    pub hide_collections: bool,
    /// Moderation state (Mastodon's `suspended_at`/`silenced_at`/
    /// `sensitized_at`): `None` means the state is off, `Some` records when a
    /// moderator applied it. See [`Account::suspended`] et al.
    pub suspended_at: Option<OffsetDateTime>,
    pub silenced_at: Option<OffsetDateTime>,
    pub sensitized_at: Option<OffsetDateTime>,
    /// Whether a suspension was applied locally or mirrors a remote `Delete`
    /// (`'local'`/`'remote'`); `None` when not suspended.
    pub suspension_origin: Option<String>,
    /// Profile-tab settings (`toot:showMedia` / `toot:showRepliesInMedia` /
    /// `toot:showFeatured`): whether the profile shows a media tab, replies
    /// in it, and a featured tab. Default on, like Mastodon.
    pub show_media: bool,
    pub show_media_replies: bool,
    pub show_featured: bool,
    /// In-memoriam marker (`toot:memorial`); the entity attribute is emitted
    /// only when set, like `suspended`.
    pub memorial: bool,
    /// The remote actor's declared `type`: `Person`, `Service`,
    /// `Application`, `Group`, `Organization`. `None` for local accounts and
    /// legacy rows, read as `Person`. Backs the Account entity's `group` flag.
    pub actor_type: Option<String>,
}

/// Which kind of actor an `acct` lookup wants. In `ActivityPub` the actor id
/// (`uri`) is the real identity; a single `user@host` handle can resolve to
/// several actors — Lemmy serves a Person (`/u/name`) and a Group (`/c/name`)
/// under one name — so every handle lookup declares which it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorClass {
    /// Person, Service, Application, Organization — anything you mention,
    /// follow as a user, or address as `@name@host`. The Mastodon-compatible
    /// default when a handle is ambiguous.
    PersonLike,
    /// Group actors (Lemmy/Mbin/PieFed communities, FEP-1b12), addressed as
    /// `!name@host`.
    Group,
    /// No filter — every actor answering the handle (bare-handle search).
    Any,
}

impl ActorClass {
    /// The class a stored/declared `actor_type` belongs to (`NULL`/legacy rows
    /// read as Person, matching [`Account::is_group`]).
    #[must_use]
    pub fn of(actor_type: Option<&str>) -> Self {
        if actor_type == Some("Group") {
            Self::Group
        } else {
            Self::PersonLike
        }
    }

    /// Whether an actor with `actor_type` satisfies this requested class.
    #[must_use]
    pub fn matches(self, actor_type: Option<&str>) -> bool {
        match self {
            Self::Any => true,
            Self::Group => actor_type == Some("Group"),
            Self::PersonLike => actor_type != Some("Group"),
        }
    }

    /// The SQL discriminator bound into the typed acct queries.
    fn sql_tag(self) -> &'static str {
        match self {
            Self::PersonLike => "person",
            Self::Group => "group",
            Self::Any => "any",
        }
    }
}

impl Account {
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.domain.is_none()
    }

    /// Whether this remotely-owned actor has a portable account hosted by the
    /// named Plamenu gateway.  Unlike [`Self::is_local`], this says where the
    /// account participates, not who owns/signs its `ActivityPub` identity.
    #[must_use]
    pub fn is_portable_on(&self, domain: &str) -> bool {
        let prefix = format!("https://{domain}/.well-known/apgateway/did:key:");
        self.uri
            .as_deref()
            .is_some_and(|uri| uri.starts_with(&prefix))
    }

    /// A handle-space account on this instance: an identity Plamenu owns or a
    /// remotely-owned portable identity registered with its gateway.
    #[must_use]
    pub fn has_local_account_on(&self, domain: &str) -> bool {
        self.is_local() || self.is_portable_on(domain)
    }

    /// A remote `Group` actor — a Lemmy/Mbin/PieFed community or Mitra group.
    #[must_use]
    pub fn is_group(&self) -> bool {
        self.actor_type.as_deref() == Some("Group")
    }

    /// The inbox delivery prefers the shared inbox when the actor has one.
    #[must_use]
    pub fn preferred_inbox(&self) -> &str {
        if self.shared_inbox_url.is_empty() {
            &self.inbox_url
        } else {
            &self.shared_inbox_url
        }
    }

    /// Moderation state (Mastodon's `suspended?`/`silenced?`/`sensitized?`).
    #[must_use]
    pub fn suspended(&self) -> bool {
        self.suspended_at.is_some()
    }

    #[must_use]
    pub fn silenced(&self) -> bool {
        self.silenced_at.is_some()
    }

    #[must_use]
    pub fn sensitized(&self) -> bool {
        self.sensitized_at.is_some()
    }
}

#[derive(Debug)]
pub struct NewLocalAccount<'a> {
    pub username: &'a str,
    pub display_name: &'a str,
    pub note: &'a str,
    pub public_key_pem: &'a str,
}

/// The immutable `ActivityPub` actor URI assigned to every newly-created local
/// account. Existing accounts keep their already-published `/users/:name` URI;
/// new accounts use the database id so a later handle change cannot move any
/// federated resource hanging off the actor.
#[must_use]
pub fn numeric_local_actor_uri(domain: &str, account_id: i64) -> String {
    format!("https://{domain}/ap/accounts/{account_id}")
}

/// Everything we persist about a remote actor after dereferencing it.
#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors Mastodon's account flags"
)]
pub struct RemoteAccountData<'a> {
    pub username: &'a str,
    pub domain: &'a str,
    pub uri: &'a str,
    pub display_name: &'a str,
    pub note: &'a str,
    pub inbox_url: &'a str,
    pub shared_inbox_url: &'a str,
    pub public_key_pem: &'a str,
    pub public_key_id: &'a str,
    pub avatar_remote_url: Option<&'a str>,
    pub header_remote_url: Option<&'a str>,
    pub avatar_description: &'a str,
    pub header_description: &'a str,
    /// Origin-published account creation time. `None` preserves an existing
    /// value or uses ingest time for a newly discovered legacy actor.
    pub created_at: Option<OffsetDateTime>,
    /// The actor's `PropertyValue` attachments, values already sanitized.
    /// Replaces the stored `account_fields` rows on every refresh; a
    /// `verified_at` stamp carries over while a field's value is unchanged.
    pub fields: Vec<FieldPair>,
    /// The actor's advertised `featured` (pinned statuses) collection.
    pub featured_collection_url: Option<&'a str>,
    /// The actor's `manuallyApprovesFollowers` flag.
    pub locked: bool,
    /// The actor's declared aliases (`alsoKnownAs`).
    pub also_known_as: &'a [String],
    /// The actor this one has migrated to (`movedTo`), if any.
    pub moved_to_uri: Option<&'a str>,
    /// The actor's human web URL (`url`), as published; `None` if absent.
    pub url: Option<&'a str>,
    /// The actor's `toot:discoverable` flag (`discoverable || false`).
    pub discoverable: bool,
    /// The actor's parsed `interactionPolicy.canFeature` bitmap.
    pub feature_approval_policy: i32,
    /// Automated account: the actor's `type` is `Service`/`Application`.
    pub is_bot: bool,
    /// The actor's `toot:indexable` flag (`indexable || false`).
    pub indexable: bool,
    /// Profile-tab settings (`toot:showMedia` et al.); `None` when the actor
    /// omits a key, keeping the stored value (Mastodon's `if @json.key?`).
    pub show_media: Option<bool>,
    pub show_media_replies: Option<bool>,
    pub show_featured: Option<bool>,
    /// The actor's `toot:memorial` flag (`memorial || false`).
    pub memorial: bool,
    /// The actor's declared `type`, already whitelisted by the caller:
    /// `Person`/`Service`/`Application`/`Group`/`Organization`. `None` stores
    /// the legacy default (read as `Person`).
    pub actor_type: Option<&'a str>,
}

/// Inserts a local account. Fails with [`DbError::UsernameTaken`] when the
/// username is already used by another local account (case-insensitively).
/// Creates a local account. Accepts any executor (a pool or an open
/// transaction), so registration can create the account, its Ed25519 keys, the
/// user row, and the confirmation-mail job as one atomic unit — a failure
/// anywhere before commit frees the reserved username instead of stranding a
/// half-made account.
pub async fn create_local<'e, E>(executor: E, new: NewLocalAccount<'_>) -> Result<Account, DbError>
where
    E: sqlx::PgExecutor<'e>,
{
    insert_local(executor, new, id::next(), None).await
}

/// Legacy-identity fixture/rollback helper that writes no plaintext private
/// column. Callers must provision the normalized encrypted key rows in the
/// same transaction before commit.
pub async fn create_local_normalized_legacy<'e, E>(
    executor: E,
    new: NewLocalAccount<'_>,
) -> Result<Account, DbError>
where
    E: sqlx::PgExecutor<'e>,
{
    insert_local(executor, new, id::next(), None).await
}

/// Creates a new local account with a handle-independent `ActivityPub` actor
/// URI. Production account/group creation uses this path; [`create_local`]
/// remains the legacy-fixture helper so migration tests can construct accounts
/// exactly as they existed before immutable IDs were introduced.
pub async fn create_local_immutable<'e, E>(
    executor: E,
    new: NewLocalAccount<'_>,
    domain: &str,
) -> Result<Account, DbError>
where
    E: sqlx::PgExecutor<'e>,
{
    let account_id = id::next();
    let actor_uri = numeric_local_actor_uri(domain, account_id);
    insert_local(executor, new, account_id, Some(&actor_uri)).await
}

async fn insert_local<'e, E>(
    executor: E,
    new: NewLocalAccount<'_>,
    account_id: i64,
    actor_uri: Option<&str>,
) -> Result<Account, DbError>
where
    E: sqlx::PgExecutor<'e>,
{
    let account = sqlx::query_as!(
        Account,
        r#"
        INSERT INTO accounts (id, username, display_name, note, public_key, uri)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, username, domain, display_name, note, public_key,
                  created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
                  avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
                  account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        "#,
        account_id,
        new.username,
        new.display_name,
        new.note,
        new.public_key_pem,
        actor_uri,
    )
    .fetch_one(executor)
    .await
    .map_err(|err| match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => DbError::UsernameTaken,
        _ => DbError::Sqlx(err),
    })?;
    Ok(account)
}

/// Persists the actor URI every legacy local account already publishes. This
/// is an application-level data migration because the canonical host lives in
/// configuration rather than `PostgreSQL`. It is idempotent and must run before
/// the server builds any actor document or signing key ID.
pub async fn backfill_legacy_local_actor_uris(pool: &PgPool, domain: &str) -> Result<u64, DbError> {
    let result = sqlx::query!(
        r#"
        UPDATE accounts
        SET uri = 'https://' || $1 || '/users/' || username
        WHERE domain IS NULL AND uri IS NULL
        "#,
        domain,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Renames only mutable local discovery metadata. The persisted actor URI and
/// every actor-key/status ID remain untouched; the former handle is reserved
/// as a human-profile redirect and cannot be reassigned to another actor.
pub async fn rename_local(
    pool: &PgPool,
    account_id: i64,
    new_username: &str,
) -> Result<bool, DbError> {
    if new_username.is_empty()
        || new_username.len() > 30
        || !new_username
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(DbError::Protocol("invalid local username".into()));
    }
    let mut tx = pool.begin().await?;
    let current = sqlx::query!(
        "SELECT username, uri FROM accounts
         WHERE id = $1 AND domain IS NULL FOR UPDATE",
        account_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(current) = current else {
        return Ok(false);
    };
    // Legacy username-ID actors cannot be renamed without changing their
    // published identity. Only opaque `/ap/accounts/:id` actors qualify.
    let immutable_suffix = format!("/ap/accounts/{account_id}");
    if !current
        .uri
        .as_deref()
        .is_some_and(|uri| uri.ends_with(&immutable_suffix))
    {
        return Err(DbError::Protocol(
            "legacy username-ID accounts cannot be handle-renamed".into(),
        ));
    }
    if current.username == new_username {
        tx.commit().await?;
        return Ok(true);
    }
    let occupied: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM accounts WHERE lower(username) = lower($1)
            UNION ALL
            SELECT 1 FROM local_handle_aliases WHERE lower(username) = lower($1)
        ) AS "occupied!"
        "#,
    )
    .bind(new_username)
    .fetch_one(&mut *tx)
    .await?;
    if occupied {
        return Err(DbError::UsernameTaken);
    }
    sqlx::query("INSERT INTO local_handle_aliases (account_id, username) VALUES ($1, $2)")
        .bind(account_id)
        .bind(current.username)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "UPDATE accounts SET username = $2, updated_at = now() WHERE id = $1",
        account_id,
        new_username,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Current account owning a former local human handle, for HTTP redirects.
pub async fn find_local_by_alias(pool: &PgPool, alias: &str) -> Result<Option<Account>, DbError> {
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT account_id FROM local_handle_aliases WHERE lower(username) = lower($1)",
    )
    .bind(alias)
    .fetch_optional(pool)
    .await?;
    match id {
        Some(id) => find_by_id(pool, id).await,
        None => Ok(None),
    }
}

/// Rollback-window public compatibility only. Production serializers and all
/// verification paths prefer normalized `actor_keys`; this column carries no
/// secret and remains readable after the private-column contract step.
pub async fn ed25519_public_key<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT ed25519_public_key FROM accounts WHERE id = $1",
        account_id,
    )
    .fetch_optional(pool)
    .await?
    .flatten())
}

/// A profile-metadata field to store, in display order: raw text for local
/// accounts, sanitized HTML for remote ones.
#[derive(Debug, Clone)]
pub struct FieldPair {
    pub name: String,
    pub value: String,
}

/// A partial profile change for a local account: `None` leaves the column
/// untouched.
#[derive(Debug, Default)]
pub struct ProfileUpdate<'a> {
    pub display_name: Option<&'a str>,
    /// Rendered HTML bio; travels together with `note_source`.
    pub note: Option<&'a str>,
    pub note_source: Option<&'a str>,
    /// Replaces the `account_fields` rows; an empty list clears them.
    pub fields: Option<Vec<FieldPair>>,
    pub avatar_file_name: Option<&'a str>,
    pub header_file_name: Option<&'a str>,
    /// Stored byte sizes for the admin `space_usage` metric; set alongside the
    /// matching `*_file_name`.
    pub avatar_file_size: Option<i64>,
    pub header_file_size: Option<i64>,
    pub locked: Option<bool>,
    pub discoverable: Option<bool>,
    pub is_bot: Option<bool>,
    pub indexable: Option<bool>,
    pub hide_collections: Option<bool>,
    pub avatar_description: Option<&'a str>,
    pub header_description: Option<&'a str>,
    /// Profile-tab settings (`showMedia`/`showRepliesInMedia`/`showFeatured`).
    pub show_media: Option<bool>,
    pub show_media_replies: Option<bool>,
    pub show_featured: Option<bool>,
}

/// Applies a profile change to a local account, returning the updated row
/// (`None` when the id is unknown or not local).
pub async fn update_local_profile(
    pool: &PgPool,
    account_id: i64,
    update: ProfileUpdate<'_>,
) -> Result<Option<Account>, DbError> {
    let mut conn = pool.begin().await?;
    let result = update_local_profile_conn(&mut conn, account_id, update).await?;
    conn.commit().await?;
    Ok(result)
}

/// Connection-scoped variant for callers assembling a transactional mutation.
pub async fn update_local_profile_conn(
    conn: &mut sqlx::PgConnection,
    account_id: i64,
    update: ProfileUpdate<'_>,
) -> Result<Option<Account>, DbError> {
    // The accounts UPDATE runs first: its row lock serializes concurrent
    // writers before either touches the `account_fields` rows.
    let account = sqlx::query_as!(
        Account,
        r#"
        UPDATE accounts SET
            display_name = coalesce($2, display_name),
            note = coalesce($3, note),
            note_source = coalesce($4, note_source),
            avatar_file_name = coalesce($5, avatar_file_name),
            header_file_name = coalesce($6, header_file_name),
            locked = coalesce($7, locked),
            discoverable = coalesce($8, discoverable),
            is_bot = coalesce($9, is_bot),
            indexable = coalesce($10, indexable),
            hide_collections = coalesce($11, hide_collections),
            avatar_description = coalesce($12, avatar_description),
            header_description = coalesce($13, header_description),
            avatar_file_size = coalesce($14, avatar_file_size),
            header_file_size = coalesce($15, header_file_size),
            show_media = coalesce($16, show_media),
            show_media_replies = coalesce($17, show_media_replies),
            show_featured = coalesce($18, show_featured),
            updated_at = now()
        WHERE id = $1 AND domain IS NULL
        RETURNING id, username, domain, display_name, note, public_key,
                  created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
                  avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
                  account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        "#,
        account_id,
        update.display_name,
        update.note,
        update.note_source,
        update.avatar_file_name,
        update.header_file_name,
        update.locked,
        update.discoverable,
        update.is_bot,
        update.indexable,
        update.hide_collections,
        update.avatar_description,
        update.header_description,
        update.avatar_file_size,
        update.header_file_size,
        update.show_media,
        update.show_media_replies,
        update.show_featured,
    )
    .fetch_optional(&mut *conn)
    .await?;
    let Some(mut account) = account else {
        return Ok(None);
    };
    if let Some(fields) = &update.fields {
        replace_fields(&mut *conn, account_id, fields).await?;
        account.fields = fields_json_of(&mut *conn, account_id).await?;
    }
    Ok(Some(account))
}

/// Clears a local account's avatar (file and alt text), returning the updated
/// row (`None` when the id is unknown or not local).
/// One stored profile-metadata row, typed — the rel="me" verifier's view of
/// [`Account::fields`].
#[derive(Debug, Clone)]
pub struct AccountField {
    pub position: i32,
    pub name: String,
    pub value: String,
    pub verified_at: Option<OffsetDateTime>,
}

/// An account's profile-metadata rows in display order.
pub async fn fields(pool: &PgPool, account_id: i64) -> Result<Vec<AccountField>, DbError> {
    let rows = sqlx::query_as!(
        AccountField,
        r#"
        SELECT position, name, value, verified_at
        FROM account_fields
        WHERE account_id = $1
        ORDER BY position
        "#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Stamps (or clears) one field's rel="me" verification. Keyed on the field's
/// position *and* content: a stamp computed against a profile that was edited
/// mid-verification matches nothing instead of clobbering the newer fields —
/// the verification job the edit re-enqueued re-checks the fresh rows.
/// Deliberately leaves `accounts.updated_at` untouched, since verification is
/// local metadata that is not federated (no spurious `Update(Actor)`).
pub async fn set_field_verified_at(
    pool: &PgPool,
    account_id: i64,
    field: &AccountField,
    verified_at: Option<OffsetDateTime>,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE account_fields SET verified_at = $5
        WHERE account_id = $1 AND position = $2 AND name = $3 AND value = $4
        "#,
        account_id,
        field.position,
        field.name,
        field.value,
        verified_at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Replaces an account's profile-metadata rows in display order (capped at
/// the schema's 20). An existing `verified_at` carries over to a field whose
/// value is unchanged (Mastodon's `fields_attributes=`); every other stamp is
/// dropped until the verification worker re-checks.
async fn replace_fields(
    tx: &mut sqlx::PgConnection,
    account_id: i64,
    fields: &[FieldPair],
) -> Result<(), sqlx::Error> {
    let old = sqlx::query!(
        "DELETE FROM account_fields WHERE account_id = $1 RETURNING value, verified_at",
        account_id,
    )
    .fetch_all(&mut *tx)
    .await?;
    let kept: Vec<&FieldPair> = fields.iter().take(20).collect();
    if kept.is_empty() {
        return Ok(());
    }
    let positions: Vec<i32> = (0..kept.len())
        .map(|position| i32::try_from(position).expect("field count is capped at 20"))
        .collect();
    let names: Vec<&str> = kept.iter().map(|field| field.name.as_str()).collect();
    let values: Vec<&str> = kept.iter().map(|field| field.value.as_str()).collect();
    let carried: Vec<Option<OffsetDateTime>> = kept
        .iter()
        .map(|field| {
            old.iter()
                .filter(|row| row.value == field.value)
                .filter_map(|row| row.verified_at)
                .max()
        })
        .collect();
    sqlx::query!(
        r#"
        INSERT INTO account_fields (account_id, position, name, value, verified_at)
        SELECT $1, v.position, v.name, v.value, v.verified_at
        FROM unnest($2::int[], $3::text[], $4::text[], $5::timestamptz[])
             AS v(position, name, value, verified_at)
        "#,
        account_id,
        &positions,
        &names as &[&str],
        &values as &[&str],
        &carried as &[Option<OffsetDateTime>],
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// The `fields` JSON for one account, rendered by the `account_fields_json()`
/// SQL helper — the single source of the shape [`Account::fields`] carries.
async fn fields_json_of(
    tx: &mut sqlx::PgConnection,
    account_id: i64,
) -> Result<serde_json::Value, sqlx::Error> {
    sqlx::query_scalar!(r#"SELECT account_fields_json($1) AS "fields!""#, account_id)
        .fetch_one(&mut *tx)
        .await
}

pub async fn clear_avatar<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
) -> Result<Option<Account>, DbError> {
    let account = sqlx::query_as!(
        Account,
        r#"
        UPDATE accounts SET
            avatar_file_name = NULL,
            avatar_file_size = NULL,
            avatar_description = '',
            updated_at = now()
        WHERE id = $1 AND domain IS NULL
        RETURNING id, username, domain, display_name, note, public_key,
                  created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
                  avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
                  account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        "#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(account)
}

/// Clears a local account's header (file and alt text), returning the updated
/// row (`None` when the id is unknown or not local).
pub async fn clear_header<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
) -> Result<Option<Account>, DbError> {
    let account = sqlx::query_as!(
        Account,
        r#"
        UPDATE accounts SET
            header_file_name = NULL,
            header_file_size = NULL,
            header_description = '',
            updated_at = now()
        WHERE id = $1 AND domain IS NULL
        RETURNING id, username, domain, display_name, note, public_key,
                  created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
                  avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
                  account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        "#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(account)
}

/// Replaces a local account's declared aliases (`alsoKnownAs`). Returns the
/// updated row, or `None` when the id is unknown or not local.
pub async fn set_aliases(
    pool: &PgPool,
    account_id: i64,
    aliases: &[String],
) -> Result<Option<Account>, DbError> {
    let account = sqlx::query_as!(
        Account,
        r#"
        UPDATE accounts SET also_known_as = $2, updated_at = now()
        WHERE id = $1 AND domain IS NULL
        RETURNING id, username, domain, display_name, note, public_key,
                  created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
                  avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
                  account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        "#,
        account_id,
        aliases,
    )
    .fetch_optional(pool)
    .await?;
    Ok(account)
}

/// Records (or clears, with `None`) the actor this account migrated to
/// (`movedTo`). Applies to local and remote accounts alike — a remote
/// account's redirect is shown to clients, a local one's is published.
pub async fn set_moved_to<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    moved_to_uri: Option<&str>,
) -> Result<Option<Account>, DbError> {
    let account = sqlx::query_as!(
        Account,
        r#"
        UPDATE accounts SET moved_to_uri = $2, updated_at = now()
        WHERE id = $1
        RETURNING id, username, domain, display_name, note, public_key,
                  created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
                  avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
                  account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        "#,
        account_id,
        moved_to_uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(account)
}

/// Inserts or refreshes a remote account, keyed by actor URI. Used both when
/// dereferencing actors and when re-fetching after a key rotation.
pub async fn upsert_remote(pool: &PgPool, data: RemoteAccountData<'_>) -> Result<Account, DbError> {
    // Snowflake id for the INSERT branch; on the ON CONFLICT update branch the
    // existing row's id is kept, so it is unused there. Generated once so a
    // race retry (see `upsert_racing`) does not burn ids.
    let new_id = id::next();
    let data = &data;
    // Boxed so the retry closure's (sizeable) query future doesn't bloat
    // every caller up the ingest chain (clippy::large_futures).
    let account = crate::upsert_racing(move || Box::pin(async move {
        // The upsert's row lock serializes concurrent ingests of the same
        // actor before either replaces the `account_fields` rows.
        let mut tx = pool.begin().await?;
        let mut account = sqlx::query_as!(
            Account,
            r#"
        INSERT INTO accounts (id, username, domain, display_name, note, public_key,
                              uri, inbox_url, shared_inbox_url, public_key_id,
                              avatar_remote_url, header_remote_url,
                              featured_collection_url, locked, also_known_as,
                              moved_to_uri, url, discoverable, feature_approval_policy,
                              is_bot, indexable, memorial,
                              show_media, show_media_replies, show_featured, actor_type,
                              avatar_description, header_description, created_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                $15, $16, $17, $18, $19, $20, $21, $22,
                coalesce($23, TRUE), coalesce($24, TRUE), coalesce($25, TRUE), $26,
                $27, $28, coalesce($29, now()))
        ON CONFLICT (uri) WHERE uri IS NOT NULL DO UPDATE SET
            username = EXCLUDED.username,
            domain = EXCLUDED.domain,
            display_name = EXCLUDED.display_name,
            note = EXCLUDED.note,
            public_key = EXCLUDED.public_key,
            inbox_url = EXCLUDED.inbox_url,
            shared_inbox_url = EXCLUDED.shared_inbox_url,
            public_key_id = EXCLUDED.public_key_id,
            avatar_remote_url = EXCLUDED.avatar_remote_url,
            header_remote_url = EXCLUDED.header_remote_url,
            featured_collection_url = EXCLUDED.featured_collection_url,
            locked = EXCLUDED.locked,
            also_known_as = EXCLUDED.also_known_as,
            moved_to_uri = EXCLUDED.moved_to_uri,
            url = EXCLUDED.url,
            discoverable = EXCLUDED.discoverable,
            feature_approval_policy = EXCLUDED.feature_approval_policy,
            is_bot = EXCLUDED.is_bot,
            indexable = EXCLUDED.indexable,
            memorial = EXCLUDED.memorial,
            actor_type = EXCLUDED.actor_type,
            avatar_description = EXCLUDED.avatar_description,
            header_description = EXCLUDED.header_description,
            created_at = coalesce($29, accounts.created_at),
            -- An actor omitting a profile-tab key keeps the stored value
            -- (Mastodon's `if @json.key?(…)`); the EXCLUDED row already
            -- coalesced fresh values over TRUE for inserts.
            show_media = coalesce($23, accounts.show_media),
            show_media_replies = coalesce($24, accounts.show_media_replies),
            show_featured = coalesce($25, accounts.show_featured),
            updated_at = now()
        RETURNING id, username, domain, display_name, note, public_key,
                  created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
                  avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
                  account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        "#,
            new_id,
            data.username,
            data.domain,
            data.display_name,
            data.note,
            data.public_key_pem,
            data.uri,
            data.inbox_url,
            data.shared_inbox_url,
            data.public_key_id,
            data.avatar_remote_url,
            data.header_remote_url,
            data.featured_collection_url,
            data.locked,
            data.also_known_as,
            data.moved_to_uri,
            data.url,
            data.discoverable,
            data.feature_approval_policy,
            data.is_bot,
            data.indexable,
            data.memorial,
            data.show_media,
            data.show_media_replies,
            data.show_featured,
            data.actor_type,
            data.avatar_description,
            data.header_description,
            data.created_at,
        )
        .fetch_one(&mut *tx)
        .await?;
        replace_fields(&mut tx, account.id, &data.fields).await?;
        account.fields = fields_json_of(&mut tx, account.id).await?;
        tx.commit().await?;
        Ok(account)
    }))
    .await?;
    Ok(account)
}

/// Whether a stored remote account's `acct ↔ actor` mapping should be
/// re-confirmed by `WebFinger`: true when it has never been webfingered
/// or was last webfingered over a day ago, mirroring Mastodon's
/// `ResolveAccountService` 1-day TTL. Not part of [`Account`] — only the
/// resolve path reads it.
pub async fn webfinger_is_stale(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    let stale = sqlx::query_scalar!(
        r#"
        SELECT (last_webfingered_at IS NULL
                OR last_webfingered_at < now() - interval '1 day') AS "stale!"
        FROM accounts WHERE id = $1
        "#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    // An unknown id can't be refreshed either way; treat it as not stale.
    Ok(stale.unwrap_or(false))
}

/// Records that a remote account's handle was just confirmed by `WebFinger`,
/// resetting its staleness clock.
pub async fn touch_webfingered(pool: &PgPool, account_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE accounts SET last_webfingered_at = now() WHERE id = $1",
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The featured-collection URL a remote actor advertised, if any. Not part
/// of [`Account`]: only the inbound `Add`/`Remove` path needs it.
pub async fn featured_url_of(pool: &PgPool, account_id: i64) -> Result<Option<String>, DbError> {
    let url = sqlx::query_scalar!(
        "SELECT featured_collection_url FROM accounts WHERE id = $1",
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(url.flatten())
}

/// Stores the collection IRIs a remote actor advertises. Local accounts keep
/// these blank; their URLs are derived from the local URL layout.
pub async fn set_collection_urls(
    pool: &PgPool,
    account_id: i64,
    followers_url: &str,
    following_url: &str,
    outbox_url: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE accounts
        SET followers_url = $2, following_url = $3, outbox_url = $4
        WHERE id = $1 AND domain IS NOT NULL
        "#,
        account_id,
        followers_url,
        following_url,
        outbox_url,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct CollectionUrls {
    pub followers_url: String,
    pub following_url: String,
    pub outbox_url: String,
}

/// The stored followers/following/outbox collection IRIs. Blank strings mean
/// the actor did not advertise a valid collection URL.
pub async fn collection_urls_of(
    pool: &PgPool,
    account_id: i64,
) -> Result<Option<CollectionUrls>, DbError> {
    let urls = sqlx::query_as!(
        CollectionUrls,
        r#"
        SELECT followers_url, following_url, outbox_url
        FROM accounts
        WHERE id = $1
        "#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(urls)
}

/// Stores the status total a remote actor's outbox reports as `totalItems`
/// (Mastodon's `statuses_count` baseline for remote accounts).
pub async fn set_remote_statuses_count(
    pool: &PgPool,
    account_id: i64,
    count: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE accounts
        SET remote_statuses_count = $2
        WHERE id = $1 AND domain IS NOT NULL
        "#,
        account_id,
        count,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Looks up a public local account by username, case-insensitively. Internal
/// protocol actors intentionally have no ordinary handle or `WebFinger` entry.
pub async fn find_local_by_username<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    username: &str,
) -> Result<Option<Account>, DbError> {
    let account = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE domain IS NULL AND NOT is_internal
          AND lower(username) = lower($1)
        "#,
        username,
    )
    .fetch_optional(pool)
    .await?;
    Ok(account)
}

/// Public-surface counterpart of [`find_local_by_username`]. The activation
/// predicate is part of the account lookup so serving a local actor does not
/// add a second database round trip to every `ActivityPub` request.
pub async fn find_public_local_by_username(
    pool: &PgPool,
    username: &str,
) -> Result<Option<Account>, DbError> {
    let account = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE domain IS NULL AND NOT is_internal
          AND lower(username) = lower($1)
          AND NOT EXISTS (
              SELECT 1
              FROM users
              WHERE users.account_id = accounts.id
                AND (NOT users.approved OR users.confirmed_at IS NULL)
          )
        "#,
        username,
    )
    .fetch_optional(pool)
    .await?;
    Ok(account)
}

/// Looks up an account in the local handle namespace.  This includes ordinary
/// Plamenu-owned actors and remotely-owned portable gateway actors; callers
/// that need a login-backed/local signing identity must keep using
/// [`find_local_by_username`].
pub async fn find_local_account_by_username(
    pool: &PgPool,
    username: &str,
) -> Result<Option<Account>, DbError> {
    let account = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE (domain IS NULL OR portable) AND NOT is_internal
          AND lower(username) = lower($1)
        "#,
        username,
    )
    .fetch_optional(pool)
    .await?;
    Ok(account)
}

/// Public-surface counterpart of [`find_local_account_by_username`]. Userless
/// portable actors remain available; login-backed local actors must have
/// completed both registration activation gates.
pub async fn find_public_local_account_by_username(
    pool: &PgPool,
    username: &str,
) -> Result<Option<Account>, DbError> {
    let account = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE (domain IS NULL OR portable) AND NOT is_internal
          AND lower(username) = lower($1)
          AND NOT EXISTS (
              SELECT 1
              FROM users
              WHERE users.account_id = accounts.id
                AND (NOT users.approved OR users.confirmed_at IS NULL)
          )
        "#,
        username,
    )
    .fetch_optional(pool)
    .await?;
    Ok(account)
}

/// Looks up any account by its `ActivityPub` actor URI (remote accounts only).
pub async fn find_by_uri<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    uri: &str,
) -> Result<Option<Account>, DbError> {
    let account = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE uri = $1
        "#,
        uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(account)
}

/// Looks up a remote account by its human-facing profile URL. Actor IDs and
/// web URLs commonly differ (Discourse category actors are one example), so
/// URL search checks both identities before attempting network discovery.
pub async fn find_by_url(pool: &PgPool, url: &str) -> Result<Option<Account>, DbError> {
    let account = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE url = $1
        ORDER BY id
        LIMIT 1
        "#,
        url,
    )
    .fetch_optional(pool)
    .await?;
    Ok(account)
}

/// Looks up known remote accounts for `username@domain`, case-insensitively,
/// filtered to the requested [`ActorClass`]. Ordered oldest-id first so a
/// singular pick is deterministic. A handle usually maps to one actor, but a
/// Lemmy-style host can answer with both a Person and a Group (distinct `uri`s),
/// so this returns every match and the caller filters by intent.
pub async fn find_remote_by_acct_class(
    pool: &PgPool,
    username: &str,
    domain: &str,
    class: ActorClass,
) -> Result<Vec<Account>, DbError> {
    let accounts = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE NOT is_internal
          AND lower(username) = lower($1) AND lower(domain) = lower($2)
          AND ($3 = 'any'
               OR ($3 = 'group'  AND actor_type = 'Group')
               OR ($3 = 'person' AND actor_type IS DISTINCT FROM 'Group'))
        ORDER BY id
        "#,
        username,
        domain,
        class.sql_tag(),
    )
    .fetch_all(pool)
    .await?;
    Ok(accounts)
}

/// Every known remote actor answering `username@domain` (both a Person and a
/// Group where a host serves both). Bare-handle search and full discovery.
pub async fn find_remote_by_acct_all(
    pool: &PgPool,
    username: &str,
    domain: &str,
) -> Result<Vec<Account>, DbError> {
    find_remote_by_acct_class(pool, username, domain, ActorClass::Any).await
}

/// The known person-like remote actor for `username@domain` (the deterministic
/// oldest one if a host somehow serves several), or `None`. This is the
/// Mastodon-compatible reading of a bare `@name@host` handle.
pub async fn find_remote_person_by_acct(
    pool: &PgPool,
    username: &str,
    domain: &str,
) -> Result<Option<Account>, DbError> {
    Ok(
        find_remote_by_acct_class(pool, username, domain, ActorClass::PersonLike)
            .await?
            .into_iter()
            .next(),
    )
}

/// The known Group remote actor for `username@domain` (a Lemmy/Mbin community),
/// or `None`. The reading of a `!name@host` handle.
pub async fn find_remote_group_by_acct(
    pool: &PgPool,
    username: &str,
    domain: &str,
) -> Result<Option<Account>, DbError> {
    Ok(
        find_remote_by_acct_class(pool, username, domain, ActorClass::Group)
            .await?
            .into_iter()
            .next(),
    )
}

/// Looks up an account by primary key (delivery jobs reference signers by id).
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn find_by_id<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
) -> Result<Option<Account>, DbError> {
    let account = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE id = $1
        "#,
        account_id,
    )
    .fetch_optional(executor)
    .await?;
    Ok(account)
}

/// Public-surface counterpart of [`find_by_id`], with registration activation
/// folded into the primary-key lookup to preserve the route's query count.
pub async fn find_publicly_available_by_id<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
) -> Result<Option<Account>, DbError> {
    let account = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE id = $1
          AND NOT EXISTS (
              SELECT 1
              FROM users
              WHERE users.account_id = accounts.id
                AND (NOT users.approved OR users.confirmed_at IS NULL)
          )
        "#,
        account_id,
    )
    .fetch_optional(executor)
    .await?;
    Ok(account)
}

/// Batch lookup by id (the `GET /api/v1/accounts?id[]=…` endpoint). Rows are
/// returned in arbitrary order; callers that need request order reorder.
pub async fn find_by_ids<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    ids: &[i64],
) -> Result<Vec<Account>, DbError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let accounts = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE id = ANY($1)
        "#,
        ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(accounts)
}

/// Public-surface counterpart of [`find_by_ids`]. The activation filter stays
/// in the batch query, so a bounded account-index request never becomes one
/// registration lookup per returned local account.
pub async fn find_publicly_available_by_ids(
    pool: &PgPool,
    ids: &[i64],
) -> Result<Vec<Account>, DbError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let accounts = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE id = ANY($1)
          AND NOT EXISTS (
              SELECT 1
              FROM users
              WHERE users.account_id = accounts.id
                AND (NOT users.approved OR users.confirmed_at IS NULL)
          )
        "#,
        ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(accounts)
}

/// Batch lookup of local accounts by username (case-insensitive), the set-based
/// form of [`find_local_by_username`] — used to resolve many actor URLs from
/// one inbound `Flag` without a query per URL. Rows are returned
/// in arbitrary order; callers match them back by lowercased username.
pub async fn find_local_by_usernames(
    pool: &PgPool,
    usernames: &[&str],
) -> Result<Vec<Account>, DbError> {
    let lowered: Vec<String> = usernames.iter().map(|u| u.to_lowercase()).collect();
    let accounts = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE domain IS NULL AND NOT is_internal
          AND lower(username) = ANY($1)
        "#,
        &lowered,
    )
    .fetch_all(pool)
    .await?;
    Ok(accounts)
}

/// Set-based form of [`find_local_account_by_username`].
pub async fn find_local_accounts_by_usernames(
    pool: &PgPool,
    usernames: &[&str],
) -> Result<Vec<Account>, DbError> {
    let lowered: Vec<String> = usernames.iter().map(|u| u.to_lowercase()).collect();
    let accounts = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE (domain IS NULL OR portable) AND NOT is_internal
          AND lower(username) = ANY($1)
        "#,
        &lowered,
    )
    .fetch_all(pool)
    .await?;
    Ok(accounts)
}

/// Batch lookup by actor URI, the set-based form of [`find_by_uri`] — resolves
/// many reported-object URIs from one inbound `Flag` in a single query (QC
/// audit #28). Rows are returned in arbitrary order; callers match by `uri`.
pub async fn find_by_uris(pool: &PgPool, uris: &[&str]) -> Result<Vec<Account>, DbError> {
    let owned: Vec<String> = uris.iter().map(|u| (*u).to_owned()).collect();
    let accounts = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE uri = ANY($1)
        "#,
        &owned,
    )
    .fetch_all(pool)
    .await?;
    Ok(accounts)
}

/// Per-author visibility inputs for a page of statuses, in one query: whether
/// the author's domain is federation-allowed (Mastodon's instance policy, same
/// `portable OR COALESCE(instance_domain_allowed(domain), false)` as
/// `account_domain_allows_federation`) and whether the author is local (a
/// `local`-visibility post is only shown when its author is local). Batched so
/// the status visibility filter avoids a per-author `account_id_visible` query.
#[derive(Debug)]
pub struct AuthorVisibility {
    pub id: i64,
    pub allowed: bool,
    pub is_local: bool,
}

pub async fn visibility_batch(
    pool: &PgPool,
    ids: &[i64],
) -> Result<Vec<AuthorVisibility>, DbError> {
    let rows = sqlx::query_as!(
        AuthorVisibility,
        r#"
        SELECT id,
               (suspended_at IS NULL
                AND (portable OR COALESCE(instance_domain_allowed(domain), false))) AS "allowed!",
               (domain IS NULL OR portable) AS "is_local!"
        FROM accounts
        WHERE id = ANY($1)
        "#,
        ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Looks up a remote account by the keyId it publishes (HTTP signature path).
pub async fn find_by_key_id(pool: &PgPool, key_id: &str) -> Result<Option<Account>, DbError> {
    let account = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
                  avatar_description, header_description,
                  suspended_at, silenced_at, sensitized_at, suspension_origin,
                  show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE public_key_id = $1
        "#,
        key_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(account)
}

/// Parameters for [`search`].
#[derive(Debug)]
pub struct AccountSearch<'a> {
    /// Raw search terms; turned into a phrase-prefix tsquery internally.
    pub terms: &'a str,
    /// Promotes accounts with a follow edge to/from the viewer, like
    /// Mastodon's advanced search.
    pub viewer: Option<i64>,
    /// Restricts results to accounts the viewer follows (plus themselves).
    pub following: bool,
    pub limit: i64,
    pub offset: i64,
}

/// Mastodon's tsquery for account search: disallowed characters blanked,
/// the rest as one quoted phrase with a trailing prefix match.
/// `None` when nothing searchable remains.
#[must_use]
pub fn search_tsquery(terms: &str) -> Option<String> {
    let cleaned: String = terms
        .chars()
        .map(|c| match c {
            '\'' | '?' | '\\' | ':' | '\u{2018}' | '\u{2019}' => ' ',
            _ => c,
        })
        .collect();
    if cleaned.trim().is_empty() {
        return None;
    }
    Some(format!("' {cleaned} ':*"))
}

/// Full-text account search over display name, username and domain,
/// ranked like Mastodon's database-backed search: accounts with a follow
/// relationship to the viewer first, then by weighted text rank.
pub async fn search(pool: &PgPool, params: &AccountSearch<'_>) -> Result<Vec<Account>, DbError> {
    let Some(tsquery) = search_tsquery(params.terms) else {
        return Ok(Vec::new());
    };
    let accounts = sqlx::query_as!(
        Account,
        r#"
        SELECT a.id, a.username, a.domain, a.display_name, a.note,
               a.public_key, a.created_at, a.updated_at, a.uri, a.inbox_url,
               a.shared_inbox_url, a.public_key_id, a.avatar_file_name,
               a.header_file_name, a.avatar_remote_url, a.header_remote_url,
               account_fields_json(a.id) AS "fields!", a.note_source, a.locked, a.also_known_as, a.moved_to_uri, a.url,
               a.discoverable, a.feature_approval_policy, a.is_bot, a.indexable, a.hide_collections,
               a.avatar_description, a.header_description,
               a.suspended_at, a.silenced_at, a.sensitized_at, a.suspension_origin,
               a.show_media, a.show_media_replies, a.show_featured, a.memorial, a.actor_type
        FROM accounts a
        LEFT JOIN follows f ON NOT f.pending
             AND ((f.account_id = a.id AND f.target_account_id = $2)
                  OR (f.account_id = $2 AND f.target_account_id = a.id))
        WHERE NOT a.is_internal
          AND a.suspended_at IS NULL
          AND NOT EXISTS (
                SELECT 1
                FROM users
                WHERE users.account_id = a.id
                  AND (NOT users.approved OR users.confirmed_at IS NULL)
          )
          AND to_tsquery('simple', $1) @@ (
                setweight(to_tsvector('simple', a.display_name), 'A') ||
                setweight(to_tsvector('simple', a.username), 'B') ||
                setweight(to_tsvector('simple', coalesce(a.domain, '')), 'C'))
          AND (NOT $3 OR a.id = $2 OR EXISTS (
                SELECT 1 FROM follows ff
                WHERE ff.account_id = $2 AND ff.target_account_id = a.id
                  AND NOT ff.pending))
        GROUP BY a.id
        ORDER BY count(f.account_id) DESC,
                 ts_rank_cd((
                     setweight(to_tsvector('simple', a.display_name), 'A') ||
                     setweight(to_tsvector('simple', a.username), 'B') ||
                     setweight(to_tsvector('simple', coalesce(a.domain, '')), 'C')),
                     to_tsquery('simple', $1), 32) DESC
        LIMIT $4 OFFSET $5
        "#,
        tsquery,
        params.viewer,
        params.following,
        params.limit,
        params.offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(accounts)
}

/// Whether an account is an implementation actor rather than an ordinary
/// discoverable person/community account.
pub async fn is_internal(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT is_internal AS "is_internal!" FROM accounts WHERE id = $1"#,
        account_id,
    )
    .fetch_optional(pool)
    .await?
    .unwrap_or(false))
}

/// Whether an account participates in this instance's local handle space.
/// This deliberately differs from actor ownership (`Account::is_local`): a
/// portable actor is client-owned but still has a full local account here.
pub async fn has_local_account(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT (domain IS NULL OR portable) AS "local!" FROM accounts WHERE id = $1"#,
        account_id,
    )
    .fetch_optional(pool)
    .await?
    .unwrap_or(false))
}

/// Whether an account may appear on public or federated surfaces.
///
/// Remote actors, hosted groups and other userless protocol actors have no
/// `users` row and are publishable as usual. A login-backed local actor stays
/// private until both halves of registration activation have completed:
/// e-mail confirmation (when configured) and moderator approval (when
/// required). Missing accounts are not publishable.
pub async fn is_publicly_available<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
) -> Result<bool, DbError> {
    let available = sqlx::query_scalar!(
        r#"
        SELECT NOT EXISTS (
            SELECT 1
            FROM users
            WHERE users.account_id = accounts.id
              AND (NOT users.approved OR users.confirmed_at IS NULL)
        ) AS "available!"
        FROM accounts
        WHERE accounts.id = $1
        "#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(available.unwrap_or(false))
}

/// Set-based form of [`is_publicly_available`]. Returned ids preserve no
/// particular order; callers use them as a membership set when resolving
/// bounded actor-addressing arrays from federation input.
pub async fn publicly_available_ids(
    pool: &PgPool,
    account_ids: &[i64],
) -> Result<Vec<i64>, DbError> {
    if account_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids = sqlx::query_scalar!(
        r#"
        SELECT accounts.id AS "id!"
        FROM accounts
        LEFT JOIN users ON users.account_id = accounts.id
        WHERE accounts.id = ANY($1)
          AND (users.id IS NULL OR (users.approved AND users.confirmed_at IS NOT NULL))
        "#,
        account_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Username reservation check for account/group creation. Deleted Webxdc
/// coordinators keep their generated username in the protocol tombstone so a
/// future ordinary account can never acquire an accidental actor alias.
pub async fn local_username_reserved(pool: &PgPool, username: &str) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM accounts
            WHERE (domain IS NULL OR portable) AND lower(username) = lower($1)
            UNION ALL
            SELECT 1 FROM webxdc_tombstones
            WHERE lower(coordinator_username) = lower($1)
        ) AS "reserved!"
        "#,
        username,
    )
    .fetch_one(pool)
    .await?)
}

/// Suspends an account, matching Mastodon's suspension semantics: stamps
/// `suspended_at`
/// and records whether the action was applied locally or mirrors an inbound
/// `Delete` (`'local'`/`'remote'`), so an unsuspend knows whether to refetch.
/// Returns whether a row changed.
pub async fn suspend(pool: &PgPool, account_id: i64, origin: &str) -> Result<bool, DbError> {
    let mut conn = pool.begin().await?;
    let result = suspend_conn(&mut conn, account_id, origin).await?;
    conn.commit().await?;
    Ok(result)
}

/// Connection-scoped variant for callers assembling a transactional mutation.
pub async fn suspend_conn(
    conn: &mut sqlx::PgConnection,
    account_id: i64,
    origin: &str,
) -> Result<bool, DbError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(account_id)
        .execute(&mut *conn)
        .await?;
    let result = sqlx::query!(
        "UPDATE accounts
         SET suspended_at = COALESCE(suspended_at, now()), suspension_origin = $2
         WHERE id = $1 AND deleted_at IS NULL",
        account_id,
        origin,
    )
    .execute(&mut *conn)
    .await?;
    if result.rows_affected() > 0 {
        sqlx::query!(
            "INSERT INTO account_deletion_requests (account_id)
             VALUES ($1) ON CONFLICT (account_id) DO NOTHING",
            account_id,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(result.rows_affected() > 0)
}

/// The account's attribution domains (Mastodon's `attributionDomains`):
/// domains authorized to attribute web content to it via
/// `fediverse:creator`. Kept out of the `Account` struct — only the actor
/// serializer, the credential source and the link-card attribution check
/// need it.
pub async fn attribution_domains<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
) -> Result<Vec<String>, DbError> {
    let domains = sqlx::query_scalar!(
        r#"SELECT attribution_domains AS "attribution_domains!" FROM accounts WHERE id = $1"#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(domains.unwrap_or_default())
}

/// Replaces the account's attribution domains (no-op when unchanged, so
/// remote refreshes don't churn `updated_at`).
pub async fn set_attribution_domains<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    domains: &[String],
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE accounts SET attribution_domains = $2
         WHERE id = $1 AND attribution_domains IS DISTINCT FROM $2",
        account_id,
        domains,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Marks a local account tombstone as permanently deleted (Mastodon's
/// "permanently unavailable"): the actor, webfinger and collections flip
/// from the blanked suspended document to `410 Gone`. Set by self-service
/// deletion alongside the suspension; a reversible moderator suspension
/// never sets it.
pub async fn mark_deleted<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE accounts SET deleted_at = now() WHERE id = $1",
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether the account carries the permanent-deletion marker. Kept out of
/// the `Account` struct: only the availability checks need it.
pub async fn is_deleted(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    let deleted = sqlx::query_scalar!(
        r#"SELECT deleted_at IS NOT NULL AS "deleted!" FROM accounts WHERE id = $1"#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(deleted.unwrap_or(false))
}

/// Whether the account is currently suspended (`suspended_at` set) — the
/// tombstone state a self-deletion or moderator suspension enters. A missing
/// account counts as "not runnable" too, so callers that guard long-running
/// work (the bulk-import worker) treat both alike: `true` means
/// "do not keep acting as this account". Cheaper than reloading the full row
/// for a mid-run recheck.
pub async fn is_suspended(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    let live = sqlx::query_scalar!(
        r#"SELECT suspended_at IS NULL AS "live!" FROM accounts WHERE id = $1"#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    // No row (hard-deleted) or a suspended row both mean "not runnable".
    Ok(!live.unwrap_or(false))
}

/// Refreshes only a remote account's RSA key material. Used while the remote
/// reports the account suspended: Mastodon keeps rotating keys through a
/// suspension but freezes profile attributes at their pre-suspension values
/// (its `update_account` skips the attribute setters while `suspended?`).
pub async fn update_remote_keys(
    pool: &PgPool,
    account_id: i64,
    public_key_pem: &str,
    public_key_id: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE accounts SET public_key = $2, public_key_id = $3, updated_at = now()
         WHERE id = $1 AND domain IS NOT NULL",
        account_id,
        public_key_pem,
        public_key_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The next batch of local accounts still awaiting their self-destruct
/// `Delete(Actor)` broadcast: local and not yet suspended, oldest first
/// (Mastodon's `Account.local.without_suspended.reorder(id: :asc)`).
/// Suspension marks progress, so the worker just re-asks until empty.
pub async fn self_destruct_pending(pool: &PgPool, limit: i64) -> Result<Vec<Account>, DbError> {
    let accounts = sqlx::query_as!(
        Account,
        r#"
        SELECT id, username, domain, display_name, note, public_key,
               created_at, updated_at, uri, inbox_url, shared_inbox_url, public_key_id,
               avatar_file_name, header_file_name, avatar_remote_url, header_remote_url,
               account_fields_json(id) AS "fields!", note_source, locked, also_known_as, moved_to_uri, url, discoverable, feature_approval_policy, is_bot, indexable, hide_collections,
               avatar_description, header_description,
               suspended_at, silenced_at, sensitized_at, suspension_origin,
               show_media, show_media_replies, show_featured, memorial, actor_type
        FROM accounts
        WHERE domain IS NULL AND suspended_at IS NULL
        ORDER BY id ASC
        LIMIT $1
        "#,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(accounts)
}

/// How many local accounts still await their self-destruct broadcast.
pub async fn count_self_destruct_pending(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!"
        FROM accounts
        WHERE domain IS NULL AND suspended_at IS NULL
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Every distinct inbox this server has ever learned about — the shared inbox
/// when one is known, the personal inbox otherwise, across *all* remote
/// accounts (Mastodon's `Account.inboxes`). The self-destruct broadcast
/// audience: not just followers, so even servers that merely fetched a local
/// account hear the deletion.
pub async fn known_remote_inboxes<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
) -> Result<Vec<String>, DbError> {
    let inboxes = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT
            CASE WHEN shared_inbox_url <> '' THEN shared_inbox_url
                 ELSE inbox_url
            END AS "inbox!"
        FROM accounts
        WHERE domain IS NOT NULL AND inbox_url <> ''
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(inboxes)
}

/// Remote inboxes that may have a recent copy or relationship involving a
/// local actor. This mirrors Mastodon's `AccountReachFinder`: followers,
/// reporters, accounts mentioned in the most recent 200 statuses, recently
/// followed/requesting accounts, and enabled relays. Suspension/restoration
/// actor Updates use this broader reach so peers that are not current
/// followers still blank or restore their cached profile.
pub async fn reach_inboxes<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
) -> Result<Vec<String>, DbError> {
    let inboxes = sqlx::query_scalar!(
        r#"
        WITH subject AS (
            SELECT COALESCE(suspended_at - interval '2 days', now() - interval '2 days') AS cutoff
            FROM accounts WHERE id = $1
        ), recent_statuses AS (
            SELECT id FROM statuses
            WHERE account_id = $1 AND deleted_at IS NULL -- STUBFILTER
              AND created_at >= (SELECT cutoff FROM subject)
            ORDER BY id DESC LIMIT 200
        ), reached(account_id) AS (
            SELECT f.account_id FROM follows f
            WHERE f.target_account_id = $1 AND NOT f.pending
            UNION
            SELECT r.account_id FROM reports r WHERE r.target_account_id = $1
            UNION
            SELECT m.account_id FROM status_mentions m
            WHERE m.status_id IN (SELECT id FROM recent_statuses)
            UNION
            SELECT f.target_account_id FROM follows f
            WHERE f.account_id = $1 AND f.created_at >= (SELECT cutoff FROM subject)
            UNION
            SELECT f.account_id FROM follows f
            WHERE f.target_account_id = $1 AND f.pending
              AND f.created_at >= (SELECT cutoff FROM subject)
        ), destinations(inbox) AS (
            SELECT CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url
                        ELSE a.inbox_url END
            FROM reached r
            JOIN accounts a ON a.id = r.account_id
            WHERE a.domain IS NOT NULL AND a.suspended_at IS NULL
              AND a.inbox_url <> ''
            UNION
            SELECT inbox_url FROM relays WHERE state = 'accepted'
        )
        SELECT DISTINCT inbox AS "inbox!" FROM destinations WHERE inbox <> ''
        "#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(inboxes)
}

/// Atomically queues an account's self-destruct `Delete(Actor)` fan-out to every
/// known inbox and marks it suspended, in one transaction. Because both effects
/// commit together, a crash mid-broadcast rolls the whole chunk back: the account
/// stays pending ([`self_destruct_pending`] selects only unsuspended locals) with
/// NO partial prefix of deliveries left to re-enqueue on the next pass. This
/// replaces the former per-inbox loop that suspended only after its last enqueue,
/// so a crash after a partial prefix left the account pending and the next pass
/// re-inserted the prefix as duplicate, un-deduplicated Deletes.
/// One batched INSERT also replaces the former O(#inboxes) round trips.
pub async fn broadcast_self_destruct(
    pool: &PgPool,
    account_id: i64,
    inbox_urls: &[String],
    activity: &serde_json::Value,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    crate::job::enqueue_many_tx(&mut *tx, account_id, inbox_urls, activity).await?;
    // Guarded on `suspended_at IS NULL` so the suspension is a genuine one-way
    // progress marker even if the same account were somehow presented twice.
    sqlx::query!(
        "UPDATE accounts SET suspended_at = now(), suspension_origin = 'local'
         WHERE id = $1 AND suspended_at IS NULL",
        account_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Lifts a suspension: clears `suspended_at` and the
/// origin marker.
pub async fn unsuspend(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    let mut conn = pool.begin().await?;
    let result = unsuspend_conn(&mut conn, account_id).await?;
    conn.commit().await?;
    Ok(result)
}

/// Connection-scoped variant for callers assembling a transactional mutation.
pub async fn unsuspend_conn(
    conn: &mut sqlx::PgConnection,
    account_id: i64,
) -> Result<bool, DbError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(account_id)
        .execute(&mut *conn)
        .await?;
    let result = sqlx::query!(
        "UPDATE accounts SET suspended_at = NULL, suspension_origin = NULL
         WHERE id = $1 AND deleted_at IS NULL",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    if result.rows_affected() > 0 {
        sqlx::query!(
            "DELETE FROM account_deletion_requests WHERE account_id = $1",
            account_id,
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query!(
            "DELETE FROM canonical_email_blocks WHERE reference_account_id = $1",
            account_id,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(result.rows_affected() > 0)
}

/// Suspended accounts whose reversible grace period elapsed, oldest first.
pub async fn expired_suspension_ids(pool: &PgPool, limit: i64) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT r.account_id AS "account_id!"
        FROM account_deletion_requests r
        JOIN accounts a ON a.id = r.account_id
        WHERE r.due_at <= now() AND a.suspended_at IS NOT NULL
          AND a.deleted_at IS NULL
        ORDER BY r.due_at, r.account_id
        LIMIT $1
        "#,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Whether an account is effectively silenced — its own `silenced_at` is set
/// *or* its home domain carries a `silence` domain block (the SQL
/// `account_silenced` helper). Used off the query path: streaming fan-out, the
/// account serializer's `limited` flag, and the web profile page.
pub async fn effectively_silenced(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    let silenced =
        sqlx::query_scalar!(r#"SELECT account_silenced($1) AS "silenced!""#, account_id,)
            .fetch_one(pool)
            .await?;
    Ok(silenced)
}

/// Silences an account, matching Mastodon's silencing semantics: stamps
/// `silenced_at`, hiding it
/// from public timelines for non-followers.
pub async fn silence(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE accounts SET silenced_at = now() WHERE id = $1",
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Lifts a silence: clears `silenced_at`.
pub async fn unsilence(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE accounts SET silenced_at = NULL WHERE id = $1",
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Marks an account sensitive, as Mastodon's sensitize action does: forces
/// all its media to
/// be treated as sensitive.
pub async fn sensitize(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE accounts SET sensitized_at = now() WHERE id = $1",
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Clears the sensitive mark.
pub async fn unsensitize(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE accounts SET sensitized_at = NULL WHERE id = $1",
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Removes any account by id (the admin `destroy`/`reject` delete); dependent
/// rows cascade through the schema.
pub async fn delete_by_id(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!("DELETE FROM accounts WHERE id = $1", account_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// [`delete_by_id`] inside an open transaction, so the admin `destroy`/`reject`
/// delete and its audit-log line commit or roll back as one unit: no account is
/// removed without its audit trail, and no trail
/// entry survives a rolled-back deletion.
pub async fn delete_by_id_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account_id: i64,
) -> Result<bool, DbError> {
    // Capture every stored file the account owns before the cascade removes its
    // media rows, so the bytes don't stay publicly retrievable after a hard
    // delete.
    let keys = crate::media_cleanup::collect_account_keys(&mut *tx, account_id).await?;
    // Cancel the account's archives explicitly (rather than relying on the FK
    // cascade, which would drop the rows but orphan their private ZIPs on disk)
    // and schedule those files for durable removal too.
    let archive_files = crate::archive::delete_for_account(&mut *tx, account_id).await?;
    let result = sqlx::query!("DELETE FROM accounts WHERE id = $1", account_id)
        .execute(&mut **tx)
        .await?;
    if result.rows_affected() > 0 {
        crate::media_cleanup::enqueue_many(&mut **tx, &keys).await?;
        crate::media_cleanup::enqueue_many(&mut **tx, &archive_files).await?;
    }
    Ok(result.rows_affected() > 0)
}

/// Atomically hard-deletes an account and appends the moderator's audit line in
/// one transaction, so the admin `destroy`/`reject` delete and its trail commit
/// or roll back together. Returns whether a row was
/// deleted. The audit line's acting account must survive the deletion (it is
/// never the account being removed — self-destroy is refused upstream), so its
/// actor snapshot resolves within the transaction.
pub async fn delete_by_id_with_audit(
    pool: &PgPool,
    account_id: i64,
    audit: crate::admin_action_log::NewActionLog<'_>,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let deleted = delete_by_id_tx(&mut tx, account_id).await?;
    crate::admin_action_log::record_tx(&mut tx, audit).await?;
    tx.commit().await?;
    Ok(deleted)
}

/// Removes a remote account (actor `Delete`); follows and statuses cascade. Its
/// cached media (avatar/header and its statuses' cached originals/previews/HLS)
/// are captured for durable file deletion in the same transaction, before the
/// cascade removes their rows — those cache files are also served publicly via
/// `GET /media/{file}`.
pub async fn delete_by_uri(pool: &PgPool, uri: &str) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let target = sqlx::query_scalar!("SELECT id FROM accounts WHERE uri = $1", uri)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(account_id) = target else {
        tx.commit().await?;
        return Ok(false);
    };
    let keys = crate::media_cleanup::collect_account_keys(&mut tx, account_id).await?;
    sqlx::query!("DELETE FROM accounts WHERE id = $1", account_id)
        .execute(&mut *tx)
        .await?;
    crate::media_cleanup::enqueue_many(&mut *tx, &keys).await?;
    tx.commit().await?;
    Ok(true)
}

/// Strips a (suspended) local account down to its tombstone — Mastodon's
/// `DeleteAccountService` purge with `reserve_username: true`. The account
/// row itself, its key material and the moderation history around it
/// (reports, strikes) stay; everything the person authored or configured
/// goes, and the profile is blanked. `delivery_jobs` is deliberately left
/// alone so a just-enqueued `Delete(Actor)` fan-out still goes out, signed
/// by the tombstone's key.
pub async fn purge_local_data(pool: &PgPool, account_id: i64) -> Result<(), DbError> {
    purge_data(pool, account_id, false, false, None)
        .await
        .map(|_| ())
}

/// Purges data on the caller's account-deletion transaction.
pub async fn purge_local_data_conn(
    conn: &mut sqlx::PgConnection,
    account_id: i64,
) -> Result<(), DbError> {
    purge_data_conn(conn, account_id, false, false, None)
        .await
        .map(|_| ())
}

/// Permanently purges one suspension only if its deletion request is due.
/// The account row is locked against `unsuspend`, so winning the grace-period
/// race is deterministic: restoration first cancels this purge; purge first
/// marks a permanent tombstone that restoration cannot lift.
pub async fn purge_expired_suspension(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    purge_data(pool, account_id, true, true, None).await
}

/// Permanently purges a temporarily suspended account and appends the admin
/// audit line in the same transaction. The account row/username/key remain as
/// a tombstone; only authored/user data is destroyed.
pub async fn purge_suspended_with_audit(
    pool: &PgPool,
    account_id: i64,
    audit: crate::admin_action_log::NewActionLog<'_>,
) -> Result<bool, DbError> {
    purge_data(pool, account_id, false, true, Some(audit)).await
}

/// Purges and audits on the transaction that queued the actor deletion.
pub async fn purge_suspended_with_audit_conn(
    conn: &mut sqlx::PgConnection,
    account_id: i64,
    audit: crate::admin_action_log::NewActionLog<'_>,
) -> Result<bool, DbError> {
    purge_data_conn(conn, account_id, false, true, Some(audit)).await
}

async fn purge_data(
    pool: &PgPool,
    account_id: i64,
    require_expired_request: bool,
    make_permanent: bool,
    audit: Option<crate::admin_action_log::NewActionLog<'_>>,
) -> Result<bool, DbError> {
    let mut conn = pool.begin().await?;
    let result = purge_data_conn(
        &mut conn,
        account_id,
        require_expired_request,
        make_permanent,
        audit,
    )
    .await?;
    conn.commit().await?;
    Ok(result)
}

/// Connection-scoped variant for callers assembling a transactional mutation.
#[allow(
    clippy::too_many_lines,
    reason = "flat per-table purge checklist, shared by deletion transactions"
)]
async fn purge_data_conn(
    conn: &mut sqlx::PgConnection,
    account_id: i64,
    require_expired_request: bool,
    make_permanent: bool,
    audit: Option<crate::admin_action_log::NewActionLog<'_>>,
) -> Result<bool, DbError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(account_id)
        .execute(&mut *conn)
        .await?;
    if require_expired_request {
        let eligible = sqlx::query_scalar!(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM account_deletion_requests r
                JOIN accounts a ON a.id = r.account_id
                WHERE r.account_id = $1 AND r.due_at <= now()
                  AND a.suspended_at IS NOT NULL AND a.deleted_at IS NULL
            ) AS "eligible!"
            "#,
            account_id,
        )
        .fetch_one(&mut *conn)
        .await?;
        if !eligible {
            return Ok(false);
        }
    } else if make_permanent {
        let eligible = sqlx::query_scalar!(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM accounts
                WHERE id = $1 AND suspended_at IS NOT NULL AND deleted_at IS NULL
            ) AS "eligible!"
            "#,
            account_id,
        )
        .fetch_one(&mut *conn)
        .await?;
        if !eligible {
            return Ok(false);
        }
    }
    crate::identity_proof::clear(&mut *conn, account_id).await?;
    // Capture every stored file the account owns — attachments, their previews
    // and HLS segments, and its avatar/header — and schedule durable deletion in
    // this same transaction, before the deletes/blanks below erase the file names
    // and leave the bytes publicly retrievable via `GET /media/{file}`.
    let media_keys = crate::media_cleanup::collect_account_keys(&mut *conn, account_id).await?;
    crate::media_cleanup::enqueue_many(&mut *conn, &media_keys).await?;
    // Cancel the account's archives in the same transaction.
    // Self-deletion keeps the account row as a tombstone, so nothing else drops
    // these rows: a scheduled/in-progress build would otherwise finalize a
    // private ZIP of the just-purged content, and a finished archive's file would
    // stay retrievable. The rows go here and their stored ZIPs are scheduled for
    // durable removal; an in-progress build that finishes after this loses the
    // conditional `archive::mark_finished` race and deletes its own object.
    let archive_files = crate::archive::delete_for_account(&mut *conn, account_id).await?;
    crate::media_cleanup::enqueue_many(&mut *conn, &archive_files).await?;
    // Authored content first: statuses cascade the interactions that hang
    // off them (favourites, bookmarks, reactions, mentions, edits, quotes,
    // polls, conversation links).
    sqlx::query!("DELETE FROM statuses WHERE account_id = $1", account_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query!(
        "DELETE FROM media_attachments WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM scheduled_statuses WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    // Their interactions with other people's content.
    sqlx::query!("DELETE FROM favourites WHERE account_id = $1", account_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query!("DELETE FROM bookmarks WHERE account_id = $1", account_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query!(
        "DELETE FROM status_reactions WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM custom_emoji_usages WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM custom_emojis WHERE owner_account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!("DELETE FROM poll_votes WHERE account_id = $1", account_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query!("DELETE FROM status_pins WHERE account_id = $1", account_id)
        .execute(&mut *conn)
        .await?;
    // The social graph, in both directions.
    sqlx::query!(
        "DELETE FROM follows WHERE account_id = $1 OR target_account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM blocks WHERE account_id = $1 OR target_account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM mutes WHERE account_id = $1 OR target_account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM account_notes WHERE account_id = $1 OR target_account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM account_endorsements WHERE account_id = $1 OR target_account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM account_domain_blocks WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM list_accounts WHERE account_id = $1",
        account_id
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!("DELETE FROM lists WHERE account_id = $1", account_id)
        .execute(&mut *conn)
        .await?;
    // Conversations and read state.
    sqlx::query!(
        "DELETE FROM account_conversations WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM conversation_mutes WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    // Notifications and their filtering config, in both directions.
    sqlx::query!(
        "DELETE FROM notifications WHERE account_id = $1 OR from_account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM notification_requests WHERE account_id = $1 OR from_account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM notification_permissions WHERE account_id = $1 OR from_account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM notification_policies WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    // Personal configuration.
    sqlx::query!(
        "DELETE FROM custom_filters WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!("DELETE FROM tag_follows WHERE account_id = $1", account_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query!(
        "DELETE FROM featured_tags WHERE account_id = $1",
        account_id
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM announcement_mutes WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM announcement_reactions WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    // FEP-7aa9 collections they own or appear in.
    sqlx::query!(
        "DELETE FROM collection_items WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!("DELETE FROM collections WHERE account_id = $1", account_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query!(
        "DELETE FROM account_media_jobs WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    // Account-owned import queue: a `scheduled`/`in_progress`
    // bulk import must not outlive the purge and recreate follows, blocks,
    // mutes, bookmarks or lists — or emit freshly signed federation — from an
    // actor peers were just told is gone. Its rows cascade with it.
    sqlx::query!("DELETE FROM bulk_imports WHERE account_id = $1", account_id)
        .execute(&mut *conn)
        .await?;
    // The tombstone keeps the accounts row, so the ON DELETE CASCADE on these
    // two tables never fires — the purge must drop them itself. A pending
    // rel="me" verification would only be a wasted claim against the
    // just-blanked profile, and a cleanup policy is personal configuration,
    // which this function's contract says goes.
    sqlx::query!(
        "DELETE FROM link_verification_jobs WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "DELETE FROM account_statuses_cleanup_policies WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    // Blank the profile (Mastodon's `purge_profile!`): only the username and
    // keys remain on the tombstone.
    sqlx::query!(
        "DELETE FROM account_fields WHERE account_id = $1",
        account_id
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        r#"
        UPDATE accounts
        SET display_name = '',
            note = '',
            note_source = '',
            avatar_file_name = NULL,
            header_file_name = NULL,
            avatar_remote_url = NULL,
            header_remote_url = NULL,
            avatar_description = '',
            header_description = '',
            also_known_as = '{}',
            moved_to_uri = NULL,
            discoverable = false,
            updated_at = now()
        WHERE id = $1
        "#,
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    if make_permanent {
        sqlx::query!("DELETE FROM users WHERE account_id = $1", account_id)
            .execute(&mut *conn)
            .await?;
        sqlx::query!(
            "UPDATE accounts SET deleted_at = COALESCE(deleted_at, now()) WHERE id = $1",
            account_id,
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query!(
            "DELETE FROM account_deletion_requests WHERE account_id = $1",
            account_id,
        )
        .execute(&mut *conn)
        .await?;
    }
    if let Some(audit) = audit {
        crate::admin_action_log::record_tx(&mut *conn, audit).await?;
    }
    Ok(true)
}

/// Number of local accounts (nodeinfo / instance stats).
pub async fn count_local(pool: &PgPool) -> Result<u64, DbError> {
    // Local groups are actors, not users — every consumer of this count
    // (nodeinfo `total_users`, instance stats, admin dashboard) wants people.
    let count = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!" FROM accounts
        WHERE (domain IS NULL OR portable) AND actor_type IS DISTINCT FROM 'Group'
        "#
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Public counterpart of [`count_local`]: pending or unconfirmed sign-ups do
/// not contribute to instance metadata before their account is publishable.
pub async fn count_public_local(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!"
        FROM accounts
        LEFT JOIN users ON users.account_id = accounts.id
        WHERE (accounts.domain IS NULL OR accounts.portable)
          AND accounts.actor_type IS DISTINCT FROM 'Group'
          AND (users.id IS NULL OR (users.approved AND users.confirmed_at IS NOT NULL))
        "#
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Number of local accounts awaiting admin approval — the dashboard's
/// pending-review card. Mirrors the admin accounts list's
/// `origin=local&status=pending` filter exactly (a local account whose user
/// row is not yet approved), so the card's count matches that list's length.
pub async fn count_local_pending(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!"
        FROM accounts a
        JOIN users u ON u.account_id = a.id
        WHERE a.domain IS NULL AND u.approved = false
        "#
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// [`count_public_local`] split into people and bots — the landing page's
/// "N users · M bots" line.
pub async fn count_local_people_and_bots(pool: &PgPool) -> Result<(u64, u64), DbError> {
    let row = sqlx::query!(
        r#"
        SELECT count(*) FILTER (WHERE NOT is_bot) AS "people!",
               count(*) FILTER (WHERE is_bot) AS "bots!"
        FROM accounts
        LEFT JOIN users ON users.account_id = accounts.id
        WHERE (accounts.domain IS NULL OR accounts.portable)
          AND accounts.actor_type IS DISTINCT FROM 'Group'
          AND (users.id IS NULL OR (users.approved AND users.confirmed_at IS NOT NULL))
        "#
    )
    .fetch_one(pool)
    .await?;
    Ok((
        u64::try_from(row.people).unwrap_or(0),
        u64::try_from(row.bots).unwrap_or(0),
    ))
}

/// Number of remote accounts on record, groups excluded — the landing page's
/// "users elsewhere we know of" counter.
pub async fn count_remote(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!" FROM accounts
        WHERE domain IS NOT NULL AND NOT portable
          AND actor_type IS DISTINCT FROM 'Group'
        "#
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Number of distinct remote domains accounts have been seen from
/// (`/api/v1/instance` `stats.domain_count`).
pub async fn count_known_domains(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(DISTINCT domain) AS "count!" FROM accounts
           WHERE domain IS NOT NULL AND NOT portable"#
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----\ntest\n-----END PUBLIC KEY-----\n";
    fn alice() -> NewLocalAccount<'static> {
        NewLocalAccount {
            username: "alice",
            display_name: "Alice",
            note: "hello",
            public_key_pem: TEST_PUBLIC_PEM,
        }
    }

    fn remote_bob() -> RemoteAccountData<'static> {
        RemoteAccountData {
            username: "bob",
            domain: "remote.example",
            uri: "https://remote.example/users/bob",
            display_name: "Bob",
            note: "",
            inbox_url: "https://remote.example/users/bob/inbox",
            shared_inbox_url: "https://remote.example/inbox",
            public_key_pem: TEST_PUBLIC_PEM,
            public_key_id: "https://remote.example/users/bob#main-key",
            avatar_remote_url: None,
            header_remote_url: None,
            avatar_description: "",
            header_description: "",
            created_at: None,
            fields: Vec::new(),
            featured_collection_url: None,
            locked: false,
            also_known_as: &[],
            moved_to_uri: None,
            url: None,
            discoverable: false,
            feature_approval_policy: 0,
            is_bot: false,
            indexable: false,
            show_media: None,
            show_media_replies: None,
            show_featured: None,
            memorial: false,
            actor_type: None,
        }
    }

    #[sqlx::test]
    async fn create_and_find_local_account(pool: PgPool) {
        let created = create_local(&pool, alice()).await.unwrap();
        assert!(created.id > 0);
        assert!(created.is_local());

        let found = find_local_by_username(&pool, "alice")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, created.id);
        assert_eq!(found.display_name, "Alice");
        assert_eq!(found.public_key, TEST_PUBLIC_PEM);
    }

    #[sqlx::test]
    async fn suspension_is_global_and_unsuspension_cancels_cleanup(pool: PgPool) {
        let alice = create_local(&pool, alice()).await.unwrap();
        let bob = create_local(
            &pool,
            NewLocalAccount {
                username: "bob",
                display_name: "Bob",
                note: "",
                public_key_pem: TEST_PUBLIC_PEM,
            },
        )
        .await
        .unwrap();

        suspend(&pool, alice.id, "local").await.unwrap();
        assert!(
            sqlx::query_scalar!(r#"SELECT account_hidden(NULL, $1) AS "hidden!""#, alice.id)
                .fetch_one(&pool)
                .await
                .unwrap()
        );
        assert!(
            sqlx::query_scalar!(
                r#"SELECT sender_filtered($1, $2) AS "filtered!""#,
                bob.id,
                alice.id,
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        );
        assert_eq!(
            sqlx::query_scalar!(
                r#"SELECT count(*) AS "count!" FROM account_deletion_requests
                   WHERE account_id = $1"#,
                alice.id,
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );

        assert!(unsuspend(&pool, alice.id).await.unwrap());
        assert!(
            !sqlx::query_scalar!(r#"SELECT account_hidden(NULL, $1) AS "hidden!""#, alice.id)
                .fetch_one(&pool)
                .await
                .unwrap()
        );
        assert_eq!(
            sqlx::query_scalar!(
                r#"SELECT count(*) AS "count!" FROM account_deletion_requests
                   WHERE account_id = $1"#,
                alice.id,
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
    }

    #[sqlx::test]
    async fn expired_suspension_becomes_permanent_tombstone(pool: PgPool) {
        let alice = create_local(&pool, alice()).await.unwrap();
        suspend(&pool, alice.id, "local").await.unwrap();
        sqlx::query!(
            "UPDATE account_deletion_requests SET due_at = now() - interval '1 second'
             WHERE account_id = $1",
            alice.id,
        )
        .execute(&pool)
        .await
        .unwrap();

        assert!(purge_expired_suspension(&pool, alice.id).await.unwrap());
        assert!(is_deleted(&pool, alice.id).await.unwrap());
        assert!(!unsuspend(&pool, alice.id).await.unwrap());
        let tombstone = find_by_id(&pool, alice.id).await.unwrap().unwrap();
        assert_eq!(tombstone.username, "alice");
        assert!(tombstone.display_name.is_empty());
        assert!(tombstone.note.is_empty());
    }

    #[sqlx::test]
    async fn purge_local_data_drops_queued_verifications_and_the_cleanup_policy(pool: PgPool) {
        let account = create_local(&pool, alice()).await.unwrap();
        crate::link_verification::enqueue(&pool, account.id)
            .await
            .unwrap();
        sqlx::query!(
            "INSERT INTO account_statuses_cleanup_policies (account_id) VALUES ($1)",
            account.id,
        )
        .execute(&pool)
        .await
        .unwrap();

        purge_local_data(&pool, account.id).await.unwrap();

        // The tombstone keeps the accounts row, so neither table's cascade
        // fires; the purge itself owes the cleanup.
        assert_eq!(
            crate::link_verification::pending_count(&pool)
                .await
                .unwrap(),
            0
        );
        assert!(
            crate::statuses_cleanup::get(&pool, account.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    async fn broadcast_self_destruct_queues_and_suspends_atomically(pool: PgPool) {
        let account = create_local(&pool, alice()).await.unwrap();
        let inboxes = [
            "https://a.example/inbox".to_owned(),
            "https://b.example/inbox".to_owned(),
        ];
        let activity = serde_json::json!({
            "id": "https://plamenu.test/users/alice#delete",
            "type": "Delete",
        });

        broadcast_self_destruct(&pool, account.id, &inboxes, &activity)
            .await
            .unwrap();

        // Both effects committed together: one delivery job per inbox, and the
        // account is suspended, so it drops out of the pending set — no later
        // pass can re-enqueue a duplicate Delete prefix (finding #42).
        assert_eq!(crate::job::pending_count(&pool).await.unwrap(), 2);
        let suspended = find_by_id(&pool, account.id).await.unwrap().unwrap();
        assert!(suspended.suspended());
        assert!(self_destruct_pending(&pool, 10).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn lookup_is_case_insensitive(pool: PgPool) {
        create_local(&pool, alice()).await.unwrap();
        let found = find_local_by_username(&pool, "ALICE").await.unwrap();
        assert!(found.is_some());
    }

    #[sqlx::test]
    async fn duplicate_username_is_rejected_case_insensitively(pool: PgPool) {
        create_local(&pool, alice()).await.unwrap();
        let dup = NewLocalAccount {
            username: "Alice",
            ..alice()
        };
        let err = create_local(&pool, dup).await.unwrap_err();
        assert!(matches!(err, DbError::UsernameTaken));
    }

    #[sqlx::test]
    async fn upsert_remote_persists_and_refreshes_bot_flag(pool: PgPool) {
        // A `Service`/`Application` actor lands as a bot.
        let bot = upsert_remote(
            &pool,
            RemoteAccountData {
                is_bot: true,
                indexable: false,
                show_media: None,
                show_media_replies: None,
                show_featured: None,
                memorial: false,
                ..remote_bob()
            },
        )
        .await
        .unwrap();
        assert!(bot.is_bot);
        let reloaded = find_by_uri(&pool, bot.uri.as_deref().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(reloaded.is_bot);

        // A later refresh that no longer advertises a bot type clears it.
        let demoted = upsert_remote(&pool, remote_bob()).await.unwrap();
        assert_eq!(demoted.id, bot.id);
        assert!(!demoted.is_bot);
    }

    #[sqlx::test]
    async fn upsert_remote_persists_and_refreshes_actor_type(pool: PgPool) {
        // A Group actor stores its type; the default helper leaves it unset.
        let group = upsert_remote(
            &pool,
            RemoteAccountData {
                actor_type: Some("Group"),
                ..remote_bob()
            },
        )
        .await
        .unwrap();
        assert_eq!(group.actor_type.as_deref(), Some("Group"));
        let reloaded = find_by_uri(&pool, group.uri.as_deref().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.actor_type.as_deref(), Some("Group"));

        // A later refresh omitting the type resets it to the legacy default.
        let refreshed = upsert_remote(&pool, remote_bob()).await.unwrap();
        assert_eq!(refreshed.id, group.id);
        assert_eq!(refreshed.actor_type, None);
    }

    #[test]
    fn actor_class_classifies_and_matches() {
        assert_eq!(ActorClass::of(Some("Group")), ActorClass::Group);
        assert_eq!(ActorClass::of(None), ActorClass::PersonLike);
        assert_eq!(ActorClass::of(Some("Service")), ActorClass::PersonLike);
        // `Any` accepts everything.
        assert!(ActorClass::Any.matches(Some("Group")));
        assert!(ActorClass::Any.matches(None));
        // `Group` accepts only Group.
        assert!(ActorClass::Group.matches(Some("Group")));
        assert!(!ActorClass::Group.matches(None));
        assert!(!ActorClass::Group.matches(Some("Person")));
        // `PersonLike` accepts everything except Group (NULL reads as Person).
        assert!(ActorClass::PersonLike.matches(None));
        assert!(ActorClass::PersonLike.matches(Some("Service")));
        assert!(!ActorClass::PersonLike.matches(Some("Group")));
    }

    /// Lemmy serves `acct:collision@lemmy.example` as BOTH a Person
    /// (`/u/collision`) and a Group (`/c/collision`) — two actor ids under one
    /// handle. The old global (username, domain) unique index rejected the
    /// second; now they coexist, distinguished by `actor_type`.
    #[sqlx::test]
    async fn remote_person_and_group_share_a_handle_by_uri(pool: PgPool) {
        let person = upsert_remote(
            &pool,
            RemoteAccountData {
                username: "collision",
                domain: "lemmy.example",
                uri: "https://lemmy.example/u/collision",
                public_key_id: "https://lemmy.example/u/collision#main-key",
                actor_type: None,
                ..remote_bob()
            },
        )
        .await
        .unwrap();
        let group = upsert_remote(
            &pool,
            RemoteAccountData {
                username: "collision",
                domain: "lemmy.example",
                uri: "https://lemmy.example/c/collision",
                public_key_id: "https://lemmy.example/c/collision#main-key",
                actor_type: Some("Group"),
                ..remote_bob()
            },
        )
        .await
        .unwrap();
        assert_ne!(person.id, group.id, "distinct rows, not an upsert collapse");

        let all = find_remote_by_acct_all(&pool, "collision", "lemmy.example")
            .await
            .unwrap();
        assert_eq!(all.len(), 2, "both actors answer the handle");

        let found_person = find_remote_person_by_acct(&pool, "collision", "lemmy.example")
            .await
            .unwrap()
            .expect("person is a match");
        assert_eq!(found_person.id, person.id);
        assert!(!found_person.is_group());

        let found_group = find_remote_group_by_acct(&pool, "collision", "lemmy.example")
            .await
            .unwrap()
            .expect("group is a match");
        assert_eq!(found_group.id, group.id);
        assert!(found_group.is_group());
    }

    /// Even two remote actors of the *same* type sharing a handle coexist by
    /// `uri`; the singular person lookup picks the deterministic oldest.
    #[sqlx::test]
    async fn same_type_remote_actors_coexist_by_uri(pool: PgPool) {
        let first = upsert_remote(
            &pool,
            RemoteAccountData {
                username: "twin",
                domain: "remote.example",
                uri: "https://remote.example/users/twin-a",
                public_key_id: "https://remote.example/users/twin-a#main-key",
                ..remote_bob()
            },
        )
        .await
        .unwrap();
        upsert_remote(
            &pool,
            RemoteAccountData {
                username: "twin",
                domain: "remote.example",
                uri: "https://remote.example/users/twin-b",
                public_key_id: "https://remote.example/users/twin-b#main-key",
                ..remote_bob()
            },
        )
        .await
        .unwrap();

        let all = find_remote_by_acct_all(&pool, "twin", "remote.example")
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        let picked = find_remote_person_by_acct(&pool, "twin", "remote.example")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(picked.id, first.id, "oldest id wins, deterministically");
    }

    /// A local user and a local group can never collide: the local namespace
    /// stays globally unique regardless of actor type.
    #[sqlx::test]
    async fn local_username_unique_across_actor_type(pool: PgPool) {
        create_local(&pool, alice()).await.unwrap();
        // A second local row under the same name (here a would-be group) is
        // rejected by the partial unique index, whatever its actor_type.
        let err = sqlx::query!(
            r#"
            INSERT INTO accounts (id, username, domain, public_key, actor_type)
            VALUES ($1, 'Alice', NULL, $2, 'Group')
            "#,
            id::next(),
            TEST_PUBLIC_PEM,
        )
        .execute(&pool)
        .await
        .unwrap_err();
        assert!(
            matches!(&err, sqlx::Error::Database(e) if e.is_unique_violation()),
            "expected a unique violation, got {err:?}"
        );
    }

    #[sqlx::test]
    async fn upsert_remote_survives_concurrent_first_insert(pool: PgPool) {
        // Two thread-ancestor backfills of the same thread upsert the *same*
        // never-seen remote actor at the same instant. The ON CONFLICT arbiter
        // is `idx_accounts_uri`; under a concurrent speculative-insert race the
        // loser can still surface a raw 23505 on that arbiter rather than
        // diverting to DO UPDATE, which used to escape as a spurious 500 (before
        // migration 0148 the lower-OID `idx_accounts_username_domain` was the
        // first collision; that index is gone now, but the retry still applies).
        // Fire a pool-saturating burst of identical upserts, gated on a barrier so
        // they hit the arbiter together, over several fresh actors to make the
        // race near-certain; every one must succeed and converge on one row.
        const CONCURRENCY: usize = 5; // the sqlx::test pool connection cap
        for round in 0..12 {
            let uri = format!("https://remote.example/users/racer{round}");
            let username = format!("racer{round}");
            let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(CONCURRENCY));
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..CONCURRENCY {
                let pool = pool.clone();
                let barrier = barrier.clone();
                let uri = uri.clone();
                let username = username.clone();
                tasks.spawn(async move {
                    let key_id = format!("{uri}#main-key");
                    barrier.wait().await;
                    upsert_remote(
                        &pool,
                        RemoteAccountData {
                            username: &username,
                            uri: &uri,
                            public_key_id: &key_id,
                            ..remote_bob()
                        },
                    )
                    .await
                });
            }

            let mut ids = Vec::new();
            while let Some(joined) = tasks.join_next().await {
                // The fix's guarantee: the race never surfaces as an error.
                let account = joined
                    .expect("task panicked")
                    .expect("upsert raced to a 500");
                ids.push(account.id);
            }
            // Every contender resolved to the single stored row.
            assert!(ids.iter().all(|&id| id == ids[0]), "diverged ids: {ids:?}");
            let rows = sqlx::query_scalar!(
                r#"SELECT count(*) AS "n!" FROM accounts WHERE uri = $1"#,
                uri,
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(rows, 1, "expected exactly one row for {uri}");
        }
    }

    #[sqlx::test]
    async fn webfinger_staleness_starts_stale_and_touch_clears(pool: PgPool) {
        let remote = upsert_remote(&pool, remote_bob()).await.unwrap();
        // Never webfingered (NULL) reads as stale.
        assert!(webfinger_is_stale(&pool, remote.id).await.unwrap());

        touch_webfingered(&pool, remote.id).await.unwrap();
        assert!(!webfinger_is_stale(&pool, remote.id).await.unwrap());

        // An unknown id is not "stale" (nothing to refresh).
        assert!(!webfinger_is_stale(&pool, -1).await.unwrap());
    }

    #[sqlx::test]
    async fn missing_account_is_none_and_counts_add_up(pool: PgPool) {
        assert!(
            find_local_by_username(&pool, "nobody")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(count_local(&pool).await.unwrap(), 0);
        create_local(&pool, alice()).await.unwrap();
        assert_eq!(count_local(&pool).await.unwrap(), 1);
    }

    #[sqlx::test]
    async fn is_suspended_reflects_state_and_missing(pool: PgPool) {
        let account = create_local(&pool, alice()).await.unwrap();
        // A fresh account is runnable.
        assert!(!is_suspended(&pool, account.id).await.unwrap());
        // Suspending flips it: the bulk-import worker's mid-run
        // recheck reads this), and lifting the suspension restores it.
        suspend(&pool, account.id, "local").await.unwrap();
        assert!(is_suspended(&pool, account.id).await.unwrap());
        unsuspend(&pool, account.id).await.unwrap();
        assert!(!is_suspended(&pool, account.id).await.unwrap());
        // A missing (hard-deleted) account counts as not-runnable.
        assert!(is_suspended(&pool, -1).await.unwrap());
    }

    #[sqlx::test]
    async fn discoverable_defaults_on_and_round_trips(pool: PgPool) {
        // Fresh local accounts opt into discovery by default (migration 0127's
        // column defaults): listed in suggestions and indexed for search.
        let local = create_local(&pool, alice()).await.unwrap();
        assert_eq!(local.discoverable, Some(true));
        assert!(local.indexable);

        // update_credentials can opt out and back in.
        let opted_out = update_local_profile(
            &pool,
            local.id,
            ProfileUpdate {
                discoverable: Some(false),
                ..ProfileUpdate::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(opted_out.discoverable, Some(false));

        // A later unrelated change leaves discoverability untouched.
        let renamed = update_local_profile(
            &pool,
            local.id,
            ProfileUpdate {
                display_name: Some("Alice II"),
                ..ProfileUpdate::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(renamed.discoverable, Some(false));

        // Remote actors carry the flag straight off the actor document.
        let remote = upsert_remote(
            &pool,
            RemoteAccountData {
                discoverable: true,
                ..remote_bob()
            },
        )
        .await
        .unwrap();
        assert_eq!(remote.discoverable, Some(true));
    }

    #[sqlx::test]
    async fn upsert_remote_is_idempotent_and_refreshes(pool: PgPool) {
        let first = upsert_remote(&pool, remote_bob()).await.unwrap();
        assert!(!first.is_local());
        assert_eq!(first.preferred_inbox(), "https://remote.example/inbox");

        let rotated = RemoteAccountData {
            display_name: "Bobby",
            public_key_pem: "-----BEGIN PUBLIC KEY-----\nrotated\n-----END PUBLIC KEY-----\n",
            ..remote_bob()
        };
        let second = upsert_remote(&pool, rotated).await.unwrap();
        assert_eq!(second.id, first.id, "same uri must keep the same row");
        assert_eq!(second.display_name, "Bobby");
        assert!(second.public_key.contains("rotated"));

        // Remote accounts never count as local users.
        assert_eq!(count_local(&pool).await.unwrap(), 0);
    }

    #[sqlx::test]
    async fn upsert_remote_stores_aliases_and_redirect(pool: PgPool) {
        let aliases = ["https://old.example/users/bob".to_owned()];
        let data = RemoteAccountData {
            also_known_as: &aliases,
            moved_to_uri: Some("https://newer.example/users/bob"),
            ..remote_bob()
        };
        let stored = upsert_remote(&pool, data).await.unwrap();
        assert_eq!(stored.also_known_as, aliases);
        assert_eq!(
            stored.moved_to_uri.as_deref(),
            Some("https://newer.example/users/bob")
        );
    }

    #[sqlx::test]
    async fn aliases_and_redirect_are_local_scoped_and_mutable(pool: PgPool) {
        let local = create_local(&pool, alice()).await.unwrap();
        assert!(local.also_known_as.is_empty());
        assert_eq!(local.moved_to_uri, None);

        // Aliases apply to local accounts only.
        let aliases = vec!["https://old.example/users/alice".to_owned()];
        let updated = set_aliases(&pool, local.id, &aliases)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.also_known_as, aliases);
        let remote = upsert_remote(&pool, remote_bob()).await.unwrap();
        assert!(
            set_aliases(&pool, remote.id, &aliases)
                .await
                .unwrap()
                .is_none(),
            "remote accounts have no aliases of their own"
        );

        // The redirect can be set and cleared, on any account kind.
        let moved = set_moved_to(&pool, local.id, Some("https://new.example/users/alice"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            moved.moved_to_uri.as_deref(),
            Some("https://new.example/users/alice")
        );
        let cleared = set_moved_to(&pool, local.id, None).await.unwrap().unwrap();
        assert_eq!(cleared.moved_to_uri, None);
    }

    #[sqlx::test]
    async fn profile_update_is_partial_and_local_only(pool: PgPool) {
        let local = create_local(&pool, alice()).await.unwrap();
        let updated = update_local_profile(
            &pool,
            local.id,
            ProfileUpdate {
                display_name: Some("Alice in Chains"),
                note: Some("<p>rocker</p>"),
                note_source: Some("rocker"),
                fields: Some(vec![FieldPair {
                    name: "Band".to_owned(),
                    value: "AiC".to_owned(),
                }]),
                ..ProfileUpdate::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.display_name, "Alice in Chains");
        assert_eq!(updated.note, "<p>rocker</p>");
        assert_eq!(updated.note_source, "rocker");
        assert_eq!(updated.fields[0]["name"], "Band");

        // A later avatar-only change leaves the rest untouched.
        let updated = update_local_profile(
            &pool,
            local.id,
            ProfileUpdate {
                avatar_file_name: Some("1.png"),
                ..ProfileUpdate::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.display_name, "Alice in Chains");
        assert_eq!(updated.avatar_file_name.as_deref(), Some("1.png"));

        // Remote accounts are not editable through this path.
        let remote = upsert_remote(&pool, remote_bob()).await.unwrap();
        assert!(
            update_local_profile(&pool, remote.id, ProfileUpdate::default())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    async fn fields_replace_carries_verification_by_value(pool: PgPool) {
        let pair = |name: &str, value: &str| FieldPair {
            name: name.to_owned(),
            value: value.to_owned(),
        };
        let with_fields = |fields: Vec<FieldPair>| ProfileUpdate {
            fields: Some(fields),
            ..ProfileUpdate::default()
        };
        let local = create_local(&pool, alice()).await.unwrap();
        update_local_profile(
            &pool,
            local.id,
            with_fields(vec![
                pair("Site", "https://example.com"),
                pair("Blog", "https://blog.example.com"),
            ]),
        )
        .await
        .unwrap()
        .unwrap();

        // The verifier stamps the first field.
        let rows = fields(&pool, local.id).await.unwrap();
        assert_eq!(rows.len(), 2);
        let stamp = OffsetDateTime::now_utc();
        set_field_verified_at(&pool, local.id, &rows[0], Some(stamp))
            .await
            .unwrap();

        // Renaming a field keeps the stamp while its value is unchanged;
        // editing the value drops it.
        let updated = update_local_profile(
            &pool,
            local.id,
            with_fields(vec![
                pair("Website", "https://example.com"),
                pair("Blog", "https://blog.example.net"),
            ]),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(updated.fields[0]["verified_at"].is_string());
        assert!(updated.fields[1].get("verified_at").is_none());

        // A stamp computed against fields that changed mid-verification
        // matches no row and is dropped.
        let stale = AccountField {
            position: 1,
            name: "Blog".to_owned(),
            value: "https://blog.example.com".to_owned(),
            verified_at: None,
        };
        set_field_verified_at(&pool, local.id, &stale, Some(stamp))
            .await
            .unwrap();
        let rows = fields(&pool, local.id).await.unwrap();
        assert_eq!(rows[1].verified_at, None);
    }

    #[sqlx::test]
    async fn find_and_delete_by_uri(pool: PgPool) {
        upsert_remote(&pool, remote_bob()).await.unwrap();
        let found = find_by_uri(&pool, "https://remote.example/users/bob")
            .await
            .unwrap();
        assert!(found.is_some());

        assert!(
            delete_by_uri(&pool, "https://remote.example/users/bob")
                .await
                .unwrap()
        );
        assert!(
            find_by_uri(&pool, "https://remote.example/users/bob")
                .await
                .unwrap()
                .is_none()
        );
        // Deleting again is a no-op.
        assert!(
            !delete_by_uri(&pool, "https://remote.example/users/bob")
                .await
                .unwrap()
        );
    }

    #[sqlx::test]
    async fn search_matches_ranks_and_filters(pool: PgPool) {
        let local = create_local(&pool, alice()).await.unwrap();
        let remote = upsert_remote(
            &pool,
            RemoteAccountData {
                username: "alicia",
                display_name: "Alice Cooper",
                ..remote_bob()
            },
        )
        .await
        .unwrap();
        let viewer = create_local(
            &pool,
            NewLocalAccount {
                username: "viewer",
                display_name: "The Viewer",
                ..alice()
            },
        )
        .await
        .unwrap();

        // Prefix search over display names and usernames hits both Alices.
        let hits = search(
            &pool,
            &AccountSearch {
                terms: "ali",
                viewer: None,
                following: false,
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
        let ids: Vec<i64> = hits.iter().map(|a| a.id).collect();
        assert!(ids.contains(&local.id) && ids.contains(&remote.id));

        // A follow relationship pushes the followed account to the front.
        crate::follow::create(&pool, viewer.id, remote.id, None)
            .await
            .unwrap();
        let hits = search(
            &pool,
            &AccountSearch {
                terms: "ali",
                viewer: Some(viewer.id),
                following: false,
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
        assert_eq!(hits[0].id, remote.id);

        // `following` restricts to followed accounts only.
        let hits = search(
            &pool,
            &AccountSearch {
                terms: "ali",
                viewer: Some(viewer.id),
                following: true,
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
        assert_eq!(hits.iter().map(|a| a.id).collect::<Vec<_>>(), [remote.id]);

        // Unsearchable queries (nothing but stripped characters) are empty.
        assert!(search_tsquery("?:'").is_none());
        let none = search(
            &pool,
            &AccountSearch {
                terms: "?",
                viewer: None,
                following: false,
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
        assert!(none.is_empty());
    }

    #[sqlx::test]
    async fn preferred_inbox_falls_back_to_personal_inbox(pool: PgPool) {
        let no_shared = RemoteAccountData {
            shared_inbox_url: "",
            ..remote_bob()
        };
        let account = upsert_remote(&pool, no_shared).await.unwrap();
        assert_eq!(
            account.preferred_inbox(),
            "https://remote.example/users/bob/inbox"
        );
    }

    #[sqlx::test]
    async fn immutable_local_handle_rename_preserves_actor_and_reserves_alias(pool: PgPool) {
        let account = create_local_immutable(&pool, alice(), "plamenu.test")
            .await
            .unwrap();
        let actor_uri = account.uri.clone();
        assert!(rename_local(&pool, account.id, "alice_new").await.unwrap());
        let renamed = find_by_id(&pool, account.id).await.unwrap().unwrap();
        assert_eq!(renamed.username, "alice_new");
        assert_eq!(renamed.uri, actor_uri);
        assert_eq!(
            find_local_by_alias(&pool, "alice")
                .await
                .unwrap()
                .unwrap()
                .id,
            account.id
        );

        let second = NewLocalAccount {
            username: "bob",
            ..alice()
        };
        let second = create_local_immutable(&pool, second, "plamenu.test")
            .await
            .unwrap();
        assert!(matches!(
            rename_local(&pool, second.id, "alice").await,
            Err(DbError::UsernameTaken)
        ));

        let legacy = create_local(
            &pool,
            NewLocalAccount {
                username: "legacy",
                ..alice()
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            rename_local(&pool, legacy.id, "legacy_new").await,
            Err(DbError::Protocol(_))
        ));
    }
}
