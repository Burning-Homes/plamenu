//! Custom emoji. Remote and instance-wide rows retain Mastodon's domain
//! namespace; local users additionally own a personal namespace that shadows
//! the instance catalog. Stable origins deduplicate borrowed/promoted copies,
//! while owner-scoped aliases preserve already-published renamed references.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// A stored custom emoji.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CustomEmoji {
    pub id: i64,
    pub shortcode: String,
    /// `None` for local emoji.
    pub domain: Option<String>,
    /// The remote emoji's `ActivityPub` id.
    pub uri: Option<String>,
    /// Remote image URL (served by its origin).
    pub image_remote_url: Option<String>,
    /// Local upload (served from `/media/{file}`).
    pub image_file_name: Option<String>,
    pub image_content_type: Option<String>,
    pub disabled: bool,
    pub visible_in_picker: bool,
    /// Admin-assigned picker category (local emoji only); `None` =
    /// uncategorized.
    pub category: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// A remote emoji parsed from an inbound `Emoji` tag entry.
#[derive(Debug)]
pub struct RemoteEmojiData<'a> {
    pub shortcode: &'a str,
    pub domain: &'a str,
    pub uri: Option<&'a str>,
    pub image_remote_url: &'a str,
    /// The tag's `updated` timestamp, when it carried a parseable one.
    pub updated: Option<OffsetDateTime>,
}

/// Records a remote emoji, with Mastodon's overwrite rule: an existing row
/// is only touched when the image URL changed or the sender vouches a
/// fresher version (`updated` at or after our stored `updated_at`) —
/// otherwise a replayed old tag could roll the image back.
pub async fn upsert_remote(pool: &PgPool, data: RemoteEmojiData<'_>) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let row_id = id::next();
    let origin_id = if let Some(uri) = data.uri {
        sqlx::query_scalar!(
            "SELECT id FROM custom_emoji_origins WHERE canonical_uri = $1 ORDER BY id LIMIT 1",
            uri
        )
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(row_id)
    } else {
        row_id
    };
    sqlx::query!(
        r#"
        INSERT INTO custom_emoji_origins (id, source, canonical_uri)
        VALUES ($1, 'federated', $2)
        ON CONFLICT (id) DO NOTHING
        "#,
        origin_id,
        data.uri,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"
        INSERT INTO custom_emojis (id, shortcode, domain, uri, image_remote_url, origin_id)
        VALUES ($1, $2, $3, $4, $5, $7)
        ON CONFLICT (shortcode, domain) WHERE domain IS NOT NULL DO UPDATE
        SET uri = EXCLUDED.uri,
            image_remote_url = EXCLUDED.image_remote_url,
            -- A changed origin URL invalidates any cached copy; the media
            -- proxy re-fetches it on next view.
            image_file_name = CASE
                WHEN custom_emojis.image_remote_url IS DISTINCT FROM EXCLUDED.image_remote_url
                THEN NULL ELSE custom_emojis.image_file_name END,
            image_content_type = CASE
                WHEN custom_emojis.image_remote_url IS DISTINCT FROM EXCLUDED.image_remote_url
                THEN NULL ELSE custom_emojis.image_content_type END,
            image_file_size = CASE
                WHEN custom_emojis.image_remote_url IS DISTINCT FROM EXCLUDED.image_remote_url
                THEN NULL ELSE custom_emojis.image_file_size END,
            updated_at = now()
        WHERE custom_emojis.image_remote_url IS DISTINCT FROM EXCLUDED.image_remote_url
           OR ($6::timestamptz IS NOT NULL AND $6 >= custom_emojis.updated_at)
        "#,
        row_id,
        data.shortcode,
        data.domain,
        data.uri,
        data.image_remote_url,
        data.updated,
        origin_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Creates a local emoji from an uploaded image. `None` when the shortcode
/// is already taken locally.
pub async fn create_local(
    pool: &PgPool,
    shortcode: &str,
    image_file_name: &str,
    image_content_type: &str,
    image_file_size: i64,
    category: Option<&str>,
) -> Result<Option<CustomEmoji>, DbError> {
    let mut tx = pool.begin().await?;
    let emoji_id = id::next();
    sqlx::query!(
        "INSERT INTO custom_emoji_origins (id, source) VALUES ($1, 'local')",
        emoji_id
    )
    .execute(&mut *tx)
    .await?;
    let created = sqlx::query_as!(
        CustomEmoji,
        r#"
        INSERT INTO custom_emojis (id, shortcode, image_file_name, image_content_type, image_file_size, category, origin_id)
        VALUES ($1, $2, $3, $4, $5, $6, $1)
        ON CONFLICT (shortcode) WHERE domain IS NULL AND owner_account_id IS NULL DO NOTHING
        RETURNING id, shortcode, domain, uri, image_remote_url, image_file_name,
                  image_content_type, disabled, visible_in_picker, category, created_at, updated_at
        "#,
        emoji_id,
        shortcode,
        image_file_name,
        image_content_type,
        image_file_size,
        category,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if created.is_some() {
        tx.commit().await?;
    } else {
        tx.rollback().await?;
    }
    Ok(created)
}

/// Enabled emoji of one domain (`None` = local) matching the given
/// shortcodes, returned in the shortcodes' order — the order the text
/// mentioned them in, like Mastodon's `from_text`.
pub async fn lookup(
    pool: &PgPool,
    shortcodes: &[String],
    domain: Option<&str>,
) -> Result<Vec<CustomEmoji>, DbError> {
    if shortcodes.is_empty() {
        return Ok(Vec::new());
    }
    let mut found = sqlx::query_as!(
        CustomEmoji,
        r#"
        SELECT id, shortcode, domain, uri, image_remote_url, image_file_name,
               image_content_type, disabled, visible_in_picker, category, created_at, updated_at
        FROM custom_emojis
        WHERE shortcode = ANY($1) AND domain IS NOT DISTINCT FROM $2
          AND owner_account_id IS NULL AND NOT disabled AND NOT retired
        "#,
        shortcodes,
        domain,
    )
    .fetch_all(pool)
    .await?;
    found.sort_by_key(|emoji| shortcodes.iter().position(|code| *code == emoji.shortcode));
    Ok(found)
}

/// [`lookup`] over many `(shortcode, domain)` pairs in one query — the
/// page-wide form. `shortcodes` and `domains` are parallel arrays (a `None`
/// domain is a local emoji); rows come back unordered, so callers key them
/// by `(domain, shortcode)` and re-apply their own per-text mention order.
pub async fn lookup_many(
    pool: &PgPool,
    shortcodes: &[String],
    domains: &[Option<String>],
) -> Result<Vec<CustomEmoji>, DbError> {
    if shortcodes.is_empty() {
        return Ok(Vec::new());
    }
    let found = sqlx::query_as!(
        CustomEmoji,
        r#"
        SELECT DISTINCT e.id AS "id!", e.shortcode AS "shortcode!", e.domain, e.uri,
               e.image_remote_url, e.image_file_name, e.image_content_type,
               e.disabled AS "disabled!", e.visible_in_picker AS "visible_in_picker!",
               e.category, e.created_at AS "created_at!", e.updated_at AS "updated_at!"
        FROM custom_emojis e
        JOIN unnest($1::text[], $2::text[]) AS want(shortcode, domain)
          ON e.shortcode = want.shortcode AND e.domain IS NOT DISTINCT FROM want.domain
        WHERE e.owner_account_id IS NULL AND NOT e.disabled AND NOT e.retired
        "#,
        shortcodes,
        domains as &[Option<String>],
    )
    .fetch_all(pool)
    .await?;
    Ok(found)
}

/// The picker listing of `GET /api/v1/custom_emojis`: local, enabled,
/// `visible_in_picker` emoji.
pub async fn listed(pool: &PgPool) -> Result<Vec<CustomEmoji>, DbError> {
    let emoji = sqlx::query_as!(
        CustomEmoji,
        r#"
        SELECT id, shortcode, domain, uri, image_remote_url, image_file_name,
               image_content_type, disabled, visible_in_picker, category, created_at, updated_at
        FROM custom_emojis
        WHERE domain IS NULL AND owner_account_id IS NULL
          AND NOT disabled AND NOT retired AND visible_in_picker
        ORDER BY shortcode
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(emoji)
}

/// Every local emoji, including disabled ones — the CLI listing.
pub async fn list_local(pool: &PgPool) -> Result<Vec<CustomEmoji>, DbError> {
    let emoji = sqlx::query_as!(
        CustomEmoji,
        r#"
        SELECT id, shortcode, domain, uri, image_remote_url, image_file_name,
               image_content_type, disabled, visible_in_picker, category, created_at, updated_at
        FROM custom_emojis
        WHERE domain IS NULL AND owner_account_id IS NULL
        ORDER BY shortcode
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(emoji)
}

/// Any locally hosted emoji by id (instance-wide or personal) — the
/// `/emojis/{id}` `ActivityPub` object.
pub async fn find_local_by_id(
    pool: &PgPool,
    emoji_id: i64,
) -> Result<Option<CustomEmoji>, DbError> {
    let emoji = sqlx::query_as!(
        CustomEmoji,
        r#"
        SELECT id, shortcode, domain, uri, image_remote_url, image_file_name,
               image_content_type, disabled, visible_in_picker, category, created_at, updated_at
        FROM custom_emojis
        WHERE id = $1 AND domain IS NULL
        "#,
        emoji_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(emoji)
}

/// Any emoji (local or remote) by id — the media proxy needs the remote
/// origin URL and any cached copy to resolve `/media/proxy/emoji/{id}`.
pub async fn find_by_id(pool: &PgPool, emoji_id: i64) -> Result<Option<CustomEmoji>, DbError> {
    let emoji = sqlx::query_as!(
        CustomEmoji,
        r#"
        SELECT id, shortcode, domain, uri, image_remote_url, image_file_name,
               image_content_type, disabled, visible_in_picker, category, created_at, updated_at
        FROM custom_emojis
        WHERE id = $1
        "#,
        emoji_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(emoji)
}

/// The emoji whose federated images are exactly `remote_urls` — how Pleroma
/// emoji-reactions (which store the reused federated image URL, not an emoji
/// id) are re-associated with their rows so their reaction chips can be
/// proxied. One query for the whole set: at most one row per URL (the lowest
/// id, so repeated calls resolve the same row); URLs with no matching emoji
/// are simply absent.
pub async fn find_by_remote_image_urls(
    pool: &PgPool,
    remote_urls: &[String],
) -> Result<Vec<CustomEmoji>, DbError> {
    if remote_urls.is_empty() {
        return Ok(Vec::new());
    }
    let emoji = sqlx::query_as!(
        CustomEmoji,
        r#"
        SELECT DISTINCT ON (image_remote_url)
               id, shortcode, domain, uri, image_remote_url, image_file_name,
               image_content_type, disabled, visible_in_picker, category, created_at, updated_at
        FROM custom_emojis
        WHERE image_remote_url = ANY($1)
        ORDER BY image_remote_url, id
        "#,
        remote_urls,
    )
    .fetch_all(pool)
    .await?;
    Ok(emoji)
}

/// Records the locally-cached copy of a remote emoji's image after the media
/// proxy fetched it.
pub async fn set_image_file(
    pool: &PgPool,
    emoji_id: i64,
    file_name: &str,
    content_type: &str,
    file_size: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE custom_emojis
        SET image_file_name = $2, image_content_type = $3, image_file_size = $4,
            image_cached_at = now()
        WHERE id = $1
        "#,
        emoji_id,
        file_name,
        content_type,
        file_size,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Evicts cached images of remote emoji older than the retention period,
/// returning the freed file names (the media proxy refetches on demand).
/// Local emoji (no domain) are the operator's own uploads and never evict.
pub async fn evict_cached_images(
    pool: &PgPool,
    retention: std::time::Duration,
    limit: i64,
) -> Result<Vec<String>, DbError> {
    let files = sqlx::query_scalar!(
        r#"
        WITH due AS (
            SELECT id, image_file_name
            FROM custom_emojis
            WHERE domain IS NOT NULL
              AND image_file_name IS NOT NULL
              AND image_cached_at IS NOT NULL
              AND image_cached_at < now() - ($1 * interval '1 second')
            ORDER BY image_cached_at
            LIMIT $2
        )
        UPDATE custom_emojis e
        SET image_file_name = NULL, image_file_size = NULL, image_cached_at = NULL
        FROM due
        WHERE e.id = due.id
        RETURNING due.image_file_name AS "image_file_name!"
        "#,
        retention.as_secs_f64(),
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(files)
}

/// Updates the admin-controlled local emoji flags and picker category
/// (`None` clears it). Returns `None` when the id is unknown or points at a
/// remote emoji.
pub async fn update_local_flags(
    pool: &PgPool,
    emoji_id: i64,
    disabled: bool,
    visible_in_picker: bool,
    category: Option<&str>,
) -> Result<Option<CustomEmoji>, DbError> {
    let emoji = sqlx::query_as!(
        CustomEmoji,
        r#"
        UPDATE custom_emojis
        SET disabled = $2,
            visible_in_picker = $3,
            category = $4,
            updated_at = now()
        WHERE id = $1 AND domain IS NULL AND owner_account_id IS NULL
        RETURNING id, shortcode, domain, uri, image_remote_url, image_file_name,
                  image_content_type, disabled, visible_in_picker, category, created_at, updated_at
        "#,
        emoji_id,
        disabled,
        visible_in_picker,
        category,
    )
    .fetch_optional(pool)
    .await?;
    Ok(emoji)
}

/// Deletes a local emoji by id and returns the deleted row so callers can clean
/// up the stored file.
pub async fn delete_local_by_id(
    pool: &PgPool,
    emoji_id: i64,
) -> Result<Option<CustomEmoji>, DbError> {
    let emoji = sqlx::query_as!(
        CustomEmoji,
        r#"
        DELETE FROM custom_emojis
        WHERE id = $1 AND domain IS NULL AND owner_account_id IS NULL
        RETURNING id, shortcode, domain, uri, image_remote_url, image_file_name,
                  image_content_type, disabled, visible_in_picker, category, created_at, updated_at
        "#,
        emoji_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(emoji)
}

/// Deletes every emoji from `domain` (the domain-purge, Mastodon's
/// `clear_emojos!` — records go, not just files), returning the cached file
/// names to remove from storage.
pub async fn purge_domain(pool: &PgPool, domain: &str) -> Result<Vec<String>, DbError> {
    let files = sqlx::query_scalar!(
        r#"
        DELETE FROM custom_emojis
        WHERE domain = $1
        RETURNING image_file_name AS "image_file_name?"
        "#,
        domain,
    )
    .fetch_all(pool)
    .await?;
    Ok(files.into_iter().flatten().collect())
}

/// Deletes a local emoji by shortcode, returning whether it existed.
pub async fn delete_local(pool: &PgPool, shortcode: &str) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM custom_emojis WHERE shortcode = $1 AND domain IS NULL AND owner_account_id IS NULL",
        shortcode,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// A custom emoji with Plamenu's ownership and lineage metadata.
#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(clippy::struct_excessive_bools, reason = "database row state columns")]
pub struct ManagedCustomEmoji {
    pub id: i64,
    pub shortcode: String,
    pub domain: Option<String>,
    pub uri: Option<String>,
    pub image_remote_url: Option<String>,
    pub image_file_name: Option<String>,
    pub image_content_type: Option<String>,
    pub disabled: bool,
    pub visible_in_picker: bool,
    pub category: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub origin_id: i64,
    pub owner_account_id: Option<i64>,
    pub borrowed: bool,
    pub retired: bool,
    pub origin_source: String,
}

impl ManagedCustomEmoji {
    #[must_use]
    pub fn as_emoji(&self) -> CustomEmoji {
        CustomEmoji {
            id: self.id,
            shortcode: self.shortcode.clone(),
            domain: self.domain.clone(),
            uri: self.uri.clone(),
            image_remote_url: self.image_remote_url.clone(),
            image_file_name: self.image_file_name.clone(),
            image_content_type: self.image_content_type.clone(),
            disabled: self.disabled,
            visible_in_picker: self.visible_in_picker,
            category: self.category.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

const MANAGED_COLUMNS: &str = r"
    e.id, e.shortcode, e.domain, e.uri, e.image_remote_url,
    e.image_file_name, e.image_content_type, e.disabled,
    e.visible_in_picker, e.category, e.created_at, e.updated_at,
    e.origin_id, e.owner_account_id, e.borrowed, e.retired,
    o.source AS origin_source
";

/// Installation-wide policy for local custom emoji. Kept separate from the
/// already-wide `instance_settings` row, but edited alongside it in the admin
/// settings UI.
#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct CustomEmojiSettings {
    /// Active personal emoji allowed per local account; zero is unlimited.
    pub personal_limit: i32,
    /// Largest local upload or borrowed copy, in KiB.
    pub max_file_size_kb: i32,
}

impl CustomEmojiSettings {
    #[must_use]
    pub fn max_file_size_bytes(self) -> usize {
        usize::try_from(self.max_file_size_kb)
            .unwrap_or(512)
            .saturating_mul(1024)
    }
}

pub async fn settings(pool: &PgPool) -> Result<CustomEmojiSettings, DbError> {
    Ok(sqlx::query_as::<_, CustomEmojiSettings>(
        "SELECT personal_limit, max_file_size_kb FROM custom_emoji_settings WHERE singleton",
    )
    .fetch_one(pool)
    .await?)
}

pub async fn personal_limit(pool: &PgPool) -> Result<i32, DbError> {
    Ok(settings(pool).await?.personal_limit)
}

pub async fn set_settings(
    pool: &PgPool,
    personal_limit: i32,
    max_file_size_kb: i32,
) -> Result<CustomEmojiSettings, DbError> {
    if personal_limit < 0 {
        return Err(DbError::Protocol(
            "personal emoji limit cannot be negative".into(),
        ));
    }
    if !(1..=16_384).contains(&max_file_size_kb) {
        return Err(DbError::Protocol(
            "custom emoji file-size limit must be between 1 and 16384 KiB".into(),
        ));
    }
    Ok(sqlx::query_as::<_, CustomEmojiSettings>(
        "UPDATE custom_emoji_settings
         SET personal_limit = $1, max_file_size_kb = $2
         WHERE singleton
         RETURNING personal_limit, max_file_size_kb",
    )
    .bind(personal_limit)
    .bind(max_file_size_kb)
    .fetch_one(pool)
    .await?)
}

pub async fn set_personal_limit(pool: &PgPool, limit: i32) -> Result<i32, DbError> {
    let current = settings(pool).await?;
    Ok(set_settings(pool, limit, current.max_file_size_kb)
        .await?
        .personal_limit)
}

pub async fn find_managed_by_id(
    pool: &PgPool,
    emoji_id: i64,
) -> Result<Option<ManagedCustomEmoji>, DbError> {
    let sql = format!(
        "SELECT {MANAGED_COLUMNS} FROM custom_emojis e JOIN custom_emoji_origins o ON o.id = e.origin_id WHERE e.id = $1"
    );
    Ok(
        sqlx::query_as::<_, ManagedCustomEmoji>(sqlx::AssertSqlSafe(sql))
            .bind(emoji_id)
            .fetch_optional(pool)
            .await?,
    )
}

/// Finds the emoji represented by one locally served media key. Media keys
/// are unique in the store; retaining retired rows here lets reactions to an
/// already-published alias keep resolving to the same stable origin.
pub async fn find_managed_by_file_name(
    pool: &PgPool,
    file_name: &str,
) -> Result<Option<ManagedCustomEmoji>, DbError> {
    let sql = format!(
        "SELECT {MANAGED_COLUMNS} FROM custom_emojis e JOIN custom_emoji_origins o ON o.id = e.origin_id WHERE e.image_file_name = $1 ORDER BY e.id LIMIT 1"
    );
    Ok(
        sqlx::query_as::<_, ManagedCustomEmoji>(sqlx::AssertSqlSafe(sql))
            .bind(file_name)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn find_managed_by_ids(
    pool: &PgPool,
    emoji_ids: &[i64],
) -> Result<Vec<ManagedCustomEmoji>, DbError> {
    if emoji_ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {MANAGED_COLUMNS} FROM custom_emojis e JOIN custom_emoji_origins o ON o.id = e.origin_id WHERE e.id = ANY($1)"
    );
    Ok(
        sqlx::query_as::<_, ManagedCustomEmoji>(sqlx::AssertSqlSafe(sql))
            .bind(emoji_ids)
            .fetch_all(pool)
            .await?,
    )
}

pub async fn list_personal(
    pool: &PgPool,
    owner_account_id: i64,
) -> Result<Vec<ManagedCustomEmoji>, DbError> {
    let sql = format!(
        "SELECT {MANAGED_COLUMNS} FROM custom_emojis e JOIN custom_emoji_origins o ON o.id = e.origin_id \
         WHERE e.owner_account_id = $1 AND NOT e.retired ORDER BY e.category NULLS LAST, e.shortcode"
    );
    Ok(
        sqlx::query_as::<_, ManagedCustomEmoji>(sqlx::AssertSqlSafe(sql))
            .bind(owner_account_id)
            .fetch_all(pool)
            .await?,
    )
}

pub async fn list_personal_for_moderation(
    pool: &PgPool,
    owner_account_id: i64,
) -> Result<Vec<ManagedCustomEmoji>, DbError> {
    let sql = format!(
        "SELECT {MANAGED_COLUMNS} FROM custom_emojis e JOIN custom_emoji_origins o ON o.id = e.origin_id \
         WHERE e.owner_account_id = $1 ORDER BY e.retired, e.category NULLS LAST, e.shortcode"
    );
    Ok(
        sqlx::query_as::<_, ManagedCustomEmoji>(sqlx::AssertSqlSafe(sql))
            .bind(owner_account_id)
            .fetch_all(pool)
            .await?,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonalCreateOutcome {
    Created(i64),
    LimitReached,
    ShortcodeTaken,
    OriginAlreadyOwned,
}

struct PersonalCreate<'a> {
    shortcode: &'a str,
    image_file_name: &'a str,
    image_content_type: &'a str,
    image_file_size: i64,
    category: Option<&'a str>,
    origin_id: Option<i64>,
    borrowed: bool,
}

async fn create_personal_inner(
    pool: &PgPool,
    owner_account_id: i64,
    input: PersonalCreate<'_>,
) -> Result<PersonalCreateOutcome, DbError> {
    let mut tx = pool.begin().await?;
    // Serialize quota checks per owner. This prevents two concurrent uploads
    // from both observing the final free slot.
    sqlx::query("SELECT id FROM accounts WHERE id = $1 FOR UPDATE")
        .bind(owner_account_id)
        .fetch_one(&mut *tx)
        .await?;
    let limit = sqlx::query_scalar::<_, i32>(
        "SELECT personal_limit FROM custom_emoji_settings WHERE singleton",
    )
    .fetch_one(&mut *tx)
    .await?;
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM custom_emojis WHERE owner_account_id = $1 AND NOT retired",
    )
    .bind(owner_account_id)
    .fetch_one(&mut *tx)
    .await?;
    if limit != 0 && count >= i64::from(limit) {
        tx.rollback().await?;
        return Ok(PersonalCreateOutcome::LimitReached);
    }
    if let Some(origin_id) = input.origin_id
        && sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM custom_emojis
             WHERE origin_id = $2 AND NOT retired
               AND (owner_account_id = $1 OR (owner_account_id IS NULL AND domain IS NULL)))",
        )
        .bind(owner_account_id)
        .bind(origin_id)
        .fetch_one(&mut *tx)
        .await?
    {
        tx.rollback().await?;
        return Ok(PersonalCreateOutcome::OriginAlreadyOwned);
    }
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM custom_emojis WHERE owner_account_id = $1 AND shortcode = $2 AND NOT retired)",
    )
    .bind(owner_account_id)
    .bind(input.shortcode)
    .fetch_one(&mut *tx)
    .await?
    {
        tx.rollback().await?;
        return Ok(PersonalCreateOutcome::ShortcodeTaken);
    }

    let emoji_id = id::next();
    let origin_id = input.origin_id.unwrap_or(emoji_id);
    if input.origin_id.is_none() {
        sqlx::query("INSERT INTO custom_emoji_origins (id, source) VALUES ($1, 'local')")
            .bind(origin_id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        r"INSERT INTO custom_emojis
           (id, shortcode, image_file_name, image_content_type, image_file_size,
            category, origin_id, owner_account_id, borrowed)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(emoji_id)
    .bind(input.shortcode)
    .bind(input.image_file_name)
    .bind(input.image_content_type)
    .bind(input.image_file_size)
    .bind(input.category)
    .bind(origin_id)
    .bind(owner_account_id)
    .bind(input.borrowed)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(PersonalCreateOutcome::Created(emoji_id))
}

pub async fn create_personal_upload(
    pool: &PgPool,
    owner_account_id: i64,
    shortcode: &str,
    image_file_name: &str,
    image_content_type: &str,
    image_file_size: i64,
    category: Option<&str>,
) -> Result<PersonalCreateOutcome, DbError> {
    create_personal_inner(
        pool,
        owner_account_id,
        PersonalCreate {
            shortcode,
            image_file_name,
            image_content_type,
            image_file_size,
            category,
            origin_id: None,
            borrowed: false,
        },
    )
    .await
}

pub async fn create_personal_borrow(
    pool: &PgPool,
    owner_account_id: i64,
    source: &ManagedCustomEmoji,
    image_file_name: &str,
    image_content_type: &str,
    image_file_size: i64,
) -> Result<PersonalCreateOutcome, DbError> {
    create_personal_inner(
        pool,
        owner_account_id,
        PersonalCreate {
            shortcode: &source.shortcode,
            image_file_name,
            image_content_type,
            image_file_size,
            category: source.category.as_deref().or(source.domain.as_deref()),
            origin_id: Some(source.origin_id),
            borrowed: true,
        },
    )
    .await
}

/// Creates an instance-wide copy while preserving the source lineage. Used by
/// the distinctly privileged server-wide import action.
pub async fn create_global_borrow(
    pool: &PgPool,
    source: &ManagedCustomEmoji,
    image_file_name: &str,
    image_content_type: &str,
    image_file_size: i64,
    category: Option<&str>,
) -> Result<Option<CustomEmoji>, DbError> {
    create_global_borrow_as(
        pool,
        source,
        &source.shortcode,
        image_file_name,
        image_content_type,
        image_file_size,
        category,
    )
    .await
}

pub async fn create_global_borrow_as(
    pool: &PgPool,
    source: &ManagedCustomEmoji,
    shortcode: &str,
    image_file_name: &str,
    image_content_type: &str,
    image_file_size: i64,
    category: Option<&str>,
) -> Result<Option<CustomEmoji>, DbError> {
    let created = sqlx::query_as::<_, CustomEmoji>(
        r"INSERT INTO custom_emojis
             (id, shortcode, image_file_name, image_content_type,
              image_file_size, category, origin_id, borrowed)
           VALUES ($1, $2, $3, $4, $5, $6, $7, true)
           ON CONFLICT (shortcode)
             WHERE domain IS NULL AND owner_account_id IS NULL DO NOTHING
           RETURNING id, shortcode, domain, uri, image_remote_url,
                     image_file_name, image_content_type, disabled,
                     visible_in_picker, category, created_at, updated_at",
    )
    .bind(id::next())
    .bind(shortcode)
    .bind(image_file_name)
    .bind(image_content_type)
    .bind(image_file_size)
    .bind(category)
    .bind(source.origin_id)
    .fetch_optional(pool)
    .await?;
    Ok(created)
}

pub async fn update_personal(
    pool: &PgPool,
    owner_account_id: i64,
    emoji_id: i64,
    shortcode: &str,
    category: Option<&str>,
) -> Result<Option<ManagedCustomEmoji>, DbError> {
    let mut tx = pool.begin().await?;
    let old_shortcode = sqlx::query_scalar::<_, String>(
        "SELECT shortcode FROM custom_emojis WHERE id = $2 AND owner_account_id = $1 AND NOT borrowed AND NOT retired FOR UPDATE",
    )
    .bind(owner_account_id)
    .bind(emoji_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(old_shortcode) = old_shortcode else {
        tx.rollback().await?;
        return Ok(None);
    };
    if old_shortcode != shortcode {
        sqlx::query(
            "INSERT INTO custom_emoji_aliases (custom_emoji_id, owner_account_id, shortcode)
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(emoji_id)
        .bind(owner_account_id)
        .bind(&old_shortcode)
        .execute(&mut *tx)
        .await?;
    }
    let sql = format!(
        "WITH updated AS (UPDATE custom_emojis SET shortcode = $3, category = $4, updated_at = now() \
         WHERE id = $2 AND owner_account_id = $1 AND NOT borrowed AND NOT retired RETURNING *) \
         SELECT {MANAGED_COLUMNS} FROM updated e JOIN custom_emoji_origins o ON o.id = e.origin_id"
    );
    let updated = sqlx::query_as::<_, ManagedCustomEmoji>(sqlx::AssertSqlSafe(sql))
        .bind(owner_account_id)
        .bind(emoji_id)
        .bind(shortcode)
        .bind(category)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(updated)
}

/// Removes an emoji from its owner's collection without destroying its row;
/// already-published statuses may continue referring to the immutable id.
pub async fn retire_personal(
    pool: &PgPool,
    owner_account_id: i64,
    emoji_id: i64,
) -> Result<bool, DbError> {
    Ok(sqlx::query(
        "UPDATE custom_emojis SET retired = true, visible_in_picker = false, updated_at = now() \
         WHERE id = $2 AND owner_account_id = $1 AND NOT retired",
    )
    .bind(owner_account_id)
    .bind(emoji_id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn moderate_personal(
    pool: &PgPool,
    emoji_id: i64,
    shortcode: &str,
    category: Option<&str>,
    disabled: bool,
) -> Result<Option<ManagedCustomEmoji>, DbError> {
    let mut tx = pool.begin().await?;
    let existing: Option<(i64, String)> = sqlx::query_as(
        "SELECT owner_account_id, shortcode FROM custom_emojis
         WHERE id = $1 AND owner_account_id IS NOT NULL AND NOT retired FOR UPDATE",
    )
    .bind(emoji_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((owner_id, old_shortcode)) = existing else {
        tx.rollback().await?;
        return Ok(None);
    };
    if old_shortcode != shortcode {
        sqlx::query(
            "INSERT INTO custom_emoji_aliases (custom_emoji_id, owner_account_id, shortcode)
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(emoji_id)
        .bind(owner_id)
        .bind(&old_shortcode)
        .execute(&mut *tx)
        .await?;
    }
    let sql = format!(
        "WITH updated AS (UPDATE custom_emojis SET shortcode = $2, category = $3, disabled = $4, updated_at = now() \
         WHERE id = $1 AND owner_account_id IS NOT NULL AND NOT retired RETURNING *) \
         SELECT {MANAGED_COLUMNS} FROM updated e JOIN custom_emoji_origins o ON o.id = e.origin_id"
    );
    let updated = sqlx::query_as::<_, ManagedCustomEmoji>(sqlx::AssertSqlSafe(sql))
        .bind(emoji_id)
        .bind(shortcode)
        .bind(category)
        .bind(disabled)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(updated)
}

pub async fn retire_personal_by_moderator(pool: &PgPool, emoji_id: i64) -> Result<bool, DbError> {
    Ok(sqlx::query(
        "UPDATE custom_emojis SET retired = true, visible_in_picker = false, updated_at = now() \
         WHERE id = $1 AND owner_account_id IS NOT NULL AND NOT retired",
    )
    .bind(emoji_id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

/// Active personal emoji first, followed by instance-wide emoji. A personal
/// shortcode shadows the same instance-wide shortcode for its owner.
pub async fn listed_for_account(
    pool: &PgPool,
    owner_account_id: i64,
) -> Result<Vec<(CustomEmoji, bool)>, DbError> {
    let rows = sqlx::query_as::<
        _,
        (
            i64,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            bool,
            bool,
            Option<String>,
            OffsetDateTime,
            OffsetDateTime,
            bool,
        ),
    >(
        r"SELECT id, shortcode, domain, uri, image_remote_url, image_file_name,
                  image_content_type, disabled, visible_in_picker, category,
                  created_at, updated_at, personal
           FROM (
             SELECT e.id, e.shortcode, e.domain, e.uri, e.image_remote_url,
                    e.image_file_name, e.image_content_type, e.disabled,
                    e.visible_in_picker, e.category, e.created_at, e.updated_at,
                    true AS personal, 0 AS priority
             FROM custom_emojis e
             WHERE e.owner_account_id = $1 AND NOT e.disabled AND NOT e.retired
               AND e.visible_in_picker
             UNION ALL
             SELECT e.id, e.shortcode, e.domain, e.uri, e.image_remote_url,
                    e.image_file_name, e.image_content_type, e.disabled,
                    e.visible_in_picker, e.category, e.created_at, e.updated_at,
                    false AS personal, 1 AS priority
             FROM custom_emojis e
             WHERE e.domain IS NULL AND e.owner_account_id IS NULL
               AND NOT e.disabled AND NOT e.retired AND e.visible_in_picker
               AND NOT EXISTS (
                 SELECT 1 FROM custom_emojis mine
                 WHERE mine.owner_account_id = $1 AND mine.shortcode = e.shortcode
                   AND NOT mine.disabled AND NOT mine.retired)
           ) picker
           ORDER BY priority, category NULLS LAST, shortcode",
    )
    .bind(owner_account_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                CustomEmoji {
                    id: r.0,
                    shortcode: r.1,
                    domain: r.2,
                    uri: r.3,
                    image_remote_url: r.4,
                    image_file_name: r.5,
                    image_content_type: r.6,
                    disabled: r.7,
                    visible_in_picker: r.8,
                    category: r.9,
                    created_at: r.10,
                    updated_at: r.11,
                },
                r.12,
            )
        })
        .collect())
}

pub async fn lookup_local_for_account<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    owner_account_id: i64,
    shortcodes: &[String],
) -> Result<Vec<CustomEmoji>, DbError> {
    if shortcodes.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, CustomEmoji>(
        r"SELECT DISTINCT ON (wanted.ord)
                  e.id, e.shortcode, e.domain, e.uri, e.image_remote_url,
                  e.image_file_name, e.image_content_type, e.disabled,
                  e.visible_in_picker, e.category, e.created_at, e.updated_at
           FROM unnest($2::text[]) WITH ORDINALITY wanted(shortcode, ord)
           JOIN custom_emojis e ON e.shortcode = wanted.shortcode
           WHERE e.domain IS NULL AND NOT e.disabled AND NOT e.retired
             AND (e.owner_account_id = $1 OR e.owner_account_id IS NULL)
           ORDER BY wanted.ord, (e.owner_account_id = $1) DESC NULLS LAST",
    )
    .bind(owner_account_id)
    .bind(shortcodes)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, sqlx::FromRow)]
pub struct RequestedCustomEmoji {
    pub request_domain: Option<String>,
    pub request_owner_account_id: Option<i64>,
    pub id: i64,
    pub shortcode: String,
    pub domain: Option<String>,
    pub uri: Option<String>,
    pub image_remote_url: Option<String>,
    pub image_file_name: Option<String>,
    pub image_content_type: Option<String>,
    pub disabled: bool,
    pub visible_in_picker: bool,
    pub category: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl RequestedCustomEmoji {
    #[must_use]
    pub fn into_emoji(self) -> CustomEmoji {
        CustomEmoji {
            id: self.id,
            shortcode: self.shortcode,
            domain: self.domain,
            uri: self.uri,
            image_remote_url: self.image_remote_url,
            image_file_name: self.image_file_name,
            image_content_type: self.image_content_type,
            disabled: self.disabled,
            visible_in_picker: self.visible_in_picker,
            category: self.category,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// Page-wide lookup that preserves a local author's personal namespace. The
/// three slices are parallel; remote requests carry a domain and no owner,
/// local authored requests carry an owner, and instance-only text carries
/// neither.
pub async fn lookup_many_for_authors<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    shortcodes: &[String],
    domains: &[Option<String>],
    owner_account_ids: &[Option<i64>],
) -> Result<Vec<RequestedCustomEmoji>, DbError> {
    if shortcodes.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as::<_, RequestedCustomEmoji>(
        r"SELECT wanted.domain AS request_domain,
                  wanted.owner_account_id AS request_owner_account_id,
                  picked.id, wanted.shortcode, picked.domain, picked.uri,
                  picked.image_remote_url, picked.image_file_name,
                  picked.image_content_type, picked.disabled,
                  picked.visible_in_picker, picked.category,
                  picked.created_at, picked.updated_at
           FROM unnest($1::text[], $2::text[], $3::bigint[])
                  AS wanted(shortcode, domain, owner_account_id)
           JOIN LATERAL (
             -- Keep the three lookup namespaces separate so PostgreSQL can
             -- use their narrow partial indexes.  Folding the alias lookup
             -- into `e.shortcode = ... OR EXISTS (...)` makes a large emoji
             -- catalogue scan once for every shortcode on the page.
             SELECT candidates.*
             FROM (
               SELECT e.*, CASE WHEN e.retired THEN 1 ELSE 3 END AS match_priority
               FROM custom_emojis e
               WHERE wanted.domain IS NULL
                 AND wanted.owner_account_id IS NOT NULL
                 AND e.owner_account_id = wanted.owner_account_id
                 AND e.domain IS NULL
                 AND e.shortcode = wanted.shortcode
                 AND NOT e.disabled

               UNION ALL

               SELECT e.*, 2 AS match_priority
               FROM custom_emojis e
               WHERE e.owner_account_id IS NULL
                 AND e.shortcode = wanted.shortcode
                 AND NOT e.disabled AND NOT e.retired
                 AND ((wanted.domain IS NOT NULL AND e.domain = wanted.domain)
                      OR (wanted.domain IS NULL AND e.domain IS NULL))

               UNION ALL

               SELECT e.*, CASE WHEN e.retired THEN 1 ELSE 3 END AS match_priority
               FROM custom_emoji_aliases alias
               JOIN custom_emojis e ON e.id = alias.custom_emoji_id
               WHERE wanted.domain IS NULL
                 AND wanted.owner_account_id IS NOT NULL
                 AND alias.owner_account_id = wanted.owner_account_id
                 AND alias.shortcode = wanted.shortcode
                 AND e.owner_account_id = wanted.owner_account_id
                 AND e.domain IS NULL
                 AND NOT e.disabled
             ) candidates
             ORDER BY candidates.match_priority DESC
             LIMIT 1
           ) picked ON true",
    )
    .bind(shortcodes)
    .bind(domains)
    .bind(owner_account_ids)
    .fetch_all(pool)
    .await?)
}

pub async fn lookup_historical_local_for_account(
    pool: &PgPool,
    owner_account_id: i64,
    shortcodes: &[String],
) -> Result<Vec<CustomEmoji>, DbError> {
    let domains = vec![None; shortcodes.len()];
    let owners = vec![Some(owner_account_id); shortcodes.len()];
    Ok(lookup_many_for_authors(pool, shortcodes, &domains, &owners)
        .await?
        .into_iter()
        .map(RequestedCustomEmoji::into_emoji)
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromoteOutcome {
    Promoted(i64),
    AlreadyPromoted(i64),
    ShortcodeTaken,
    NotPersonal,
}

pub async fn promote_personal(
    pool: &PgPool,
    emoji_id: i64,
    shortcode: &str,
    category: Option<&str>,
) -> Result<PromoteOutcome, DbError> {
    let mut tx = pool.begin().await?;
    let Some((origin_id, owner_id, old_shortcode)): Option<(i64, Option<i64>, String)> = sqlx::query_as(
        "SELECT origin_id, owner_account_id, shortcode FROM custom_emojis WHERE id = $1 AND NOT retired FOR UPDATE",
    )
    .bind(emoji_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.rollback().await?;
        return Ok(PromoteOutcome::NotPersonal);
    };
    if owner_id.is_none() {
        tx.rollback().await?;
        return Ok(PromoteOutcome::NotPersonal);
    }
    if let Some(global_id) = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM custom_emojis WHERE origin_id = $1 AND domain IS NULL AND owner_account_id IS NULL AND NOT retired LIMIT 1",
    )
    .bind(origin_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        sqlx::query("UPDATE custom_emojis SET retired = true, visible_in_picker = false, updated_at = now() WHERE origin_id = $1 AND owner_account_id IS NOT NULL AND NOT retired")
            .bind(origin_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(PromoteOutcome::AlreadyPromoted(global_id));
    }
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM custom_emojis WHERE shortcode = $1 AND domain IS NULL AND owner_account_id IS NULL)",
    )
    .bind(shortcode)
    .fetch_one(&mut *tx)
    .await?
    {
        tx.rollback().await?;
        return Ok(PromoteOutcome::ShortcodeTaken);
    }
    if old_shortcode != shortcode {
        sqlx::query(
            "INSERT INTO custom_emoji_aliases (custom_emoji_id, owner_account_id, shortcode)
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(emoji_id)
        .bind(owner_id.expect("personal emoji has owner"))
        .bind(&old_shortcode)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE custom_emojis SET owner_account_id = NULL, shortcode = $2, category = $3, \
         borrowed = false, visible_in_picker = true, updated_at = now() WHERE id = $1",
    )
    .bind(emoji_id)
    .bind(shortcode)
    .bind(category)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE custom_emojis SET retired = true, visible_in_picker = false, updated_at = now() \
         WHERE origin_id = $1 AND owner_account_id IS NOT NULL AND NOT retired",
    )
    .bind(origin_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(PromoteOutcome::Promoted(emoji_id))
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TrendingCustomEmoji {
    pub origin_id: i64,
    pub emoji_id: i64,
    pub shortcode: String,
    pub category: Option<String>,
    pub owner_account_id: Option<i64>,
    pub domain: Option<String>,
    pub origin_source: String,
    pub unique_users: i64,
    pub total_uses: i64,
    pub image_remote_url: Option<String>,
    pub image_file_name: Option<String>,
    pub image_content_type: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum TrendingScope {
    Federated,
    Local,
    PersonalOnly,
}

pub async fn trending(
    pool: &PgPool,
    scope: TrendingScope,
    limit: i64,
) -> Result<Vec<TrendingCustomEmoji>, DbError> {
    let predicate = match scope {
        TrendingScope::Federated => "o.source = 'federated'",
        TrendingScope::Local => "representative.domain IS NULL",
        TrendingScope::PersonalOnly => {
            "representative.owner_account_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM custom_emojis global WHERE global.origin_id = u.origin_id AND global.domain IS NULL AND global.owner_account_id IS NULL AND NOT global.retired)"
        }
    };
    let sql = format!(
        r"SELECT u.origin_id, representative.id AS emoji_id,
                  representative.shortcode, representative.category,
                  representative.owner_account_id,
                  representative.domain, o.source AS origin_source,
                  count(DISTINCT u.account_id) AS unique_users,
                  sum(u.post_uses + u.reaction_uses) AS total_uses,
                  representative.image_remote_url,
                  representative.image_file_name,
                  representative.image_content_type
           FROM custom_emoji_usages u
           JOIN custom_emoji_origins o ON o.id = u.origin_id
           JOIN LATERAL (
             SELECT e.id, e.shortcode, e.category, e.owner_account_id, e.domain,
                    e.image_remote_url, e.image_file_name, e.image_content_type
             FROM custom_emojis e WHERE e.origin_id = u.origin_id AND NOT e.retired
             ORDER BY (e.domain IS NULL AND e.owner_account_id IS NULL AND NOT e.retired) DESC,
                      (e.owner_account_id IS NOT NULL AND NOT e.retired) DESC, e.id
             LIMIT 1
           ) representative ON true
           WHERE u.day >= CURRENT_DATE - 6 AND {predicate}
           GROUP BY u.origin_id, representative.id, representative.shortcode,
                    representative.category,
                    representative.owner_account_id, representative.domain, o.source,
                    representative.image_remote_url, representative.image_file_name,
                    representative.image_content_type
           ORDER BY unique_users DESC, total_uses DESC, u.origin_id
           LIMIT $1",
    );
    Ok(
        sqlx::query_as::<_, TrendingCustomEmoji>(sqlx::AssertSqlSafe(sql))
            .bind(limit.clamp(1, 200))
            .fetch_all(pool)
            .await?,
    )
}

async fn record_usage<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    origin_ids: &[i64],
    reaction: bool,
) -> Result<(), DbError> {
    if origin_ids.is_empty() {
        return Ok(());
    }
    let post_increment = i32::from(!reaction);
    let reaction_increment = i32::from(reaction);
    sqlx::query(
        r"INSERT INTO custom_emoji_usages
             (origin_id, day, account_id, post_uses, reaction_uses)
           SELECT DISTINCT origin_id, CURRENT_DATE, $1, $3, $4
           FROM unnest($2::bigint[]) AS origins(origin_id)
           ON CONFLICT (origin_id, day, account_id) DO UPDATE
           SET post_uses = custom_emoji_usages.post_uses + EXCLUDED.post_uses,
               reaction_uses = custom_emoji_usages.reaction_uses + EXCLUDED.reaction_uses",
    )
    .bind(account_id)
    .bind(origin_ids)
    .bind(post_increment)
    .bind(reaction_increment)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn record_post_usage(
    pool: &PgPool,
    account_id: i64,
    origin_ids: &[i64],
) -> Result<(), DbError> {
    record_usage(pool, account_id, origin_ids, false).await
}

pub async fn record_post_usage_on(
    conn: &mut crate::PgConnection,
    account_id: i64,
    origin_ids: &[i64],
) -> Result<(), DbError> {
    if origin_ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        r"INSERT INTO custom_emoji_usages
             (origin_id, day, account_id, post_uses, reaction_uses)
           SELECT DISTINCT origin_id, CURRENT_DATE, $1, 1, 0
           FROM unnest($2::bigint[]) AS origins(origin_id)
           ON CONFLICT (origin_id, day, account_id) DO UPDATE
           SET post_uses = custom_emoji_usages.post_uses + 1",
    )
    .bind(account_id)
    .bind(origin_ids)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn record_reaction_usage<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    origin_id: i64,
) -> Result<(), DbError> {
    record_usage(pool, account_id, &[origin_id], true).await
}

#[cfg(test)]
mod tests {
    #[sqlx::test]
    async fn fresh_emoji_upload_limit_can_be_overridden(pool: PgPool) {
        let initial = settings(&pool).await.unwrap();
        assert_eq!(initial.max_file_size_kb, 512);
        assert_eq!(initial.max_file_size_bytes(), 512 * 1024);
        let saved = set_settings(&pool, initial.personal_limit, 256)
            .await
            .unwrap();
        assert_eq!(saved.max_file_size_kb, 256);
    }

    use super::*;
    use crate::account::{self, NewLocalAccount};

    async fn local(pool: &PgPool, username: &str) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username,
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    fn remote(image: &str, updated: Option<OffsetDateTime>) -> RemoteEmojiData<'_> {
        RemoteEmojiData {
            shortcode: "blobcat",
            domain: "remote.example",
            uri: Some("https://remote.example/emojis/1"),
            image_remote_url: image,
            updated,
        }
    }

    #[sqlx::test]
    async fn upsert_remote_applies_mastodons_overwrite_rule(pool: PgPool) {
        upsert_remote(&pool, remote("https://remote.example/a.png", None))
            .await
            .unwrap();
        let codes = vec!["blobcat".to_owned()];
        let stored = lookup(&pool, &codes, Some("remote.example")).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].image_remote_url.as_deref(),
            Some("https://remote.example/a.png")
        );

        // Same image, no updated stamp: the row is left alone.
        let before = stored[0].updated_at;
        upsert_remote(&pool, remote("https://remote.example/a.png", None))
            .await
            .unwrap();
        let unchanged = lookup(&pool, &codes, Some("remote.example")).await.unwrap();
        assert_eq!(unchanged[0].updated_at, before);

        // A changed image always wins.
        upsert_remote(&pool, remote("https://remote.example/b.png", None))
            .await
            .unwrap();
        let changed = lookup(&pool, &codes, Some("remote.example")).await.unwrap();
        assert_eq!(
            changed[0].image_remote_url.as_deref(),
            Some("https://remote.example/b.png")
        );

        // A stale `updated` (before our stored updated_at) with the same
        // image must not bump the row.
        let stale = OffsetDateTime::now_utc() - time::Duration::days(1);
        let before = changed[0].updated_at;
        upsert_remote(&pool, remote("https://remote.example/b.png", Some(stale)))
            .await
            .unwrap();
        let kept = lookup(&pool, &codes, Some("remote.example")).await.unwrap();
        assert_eq!(kept[0].updated_at, before);

        // A fresh `updated` re-stamps even with the same image.
        let fresh = OffsetDateTime::now_utc() + time::Duration::days(1);
        upsert_remote(&pool, remote("https://remote.example/b.png", Some(fresh)))
            .await
            .unwrap();
        let bumped = lookup(&pool, &codes, Some("remote.example")).await.unwrap();
        assert!(bumped[0].updated_at > before);
    }

    #[sqlx::test]
    async fn lookup_is_domain_exact_and_skips_disabled(pool: PgPool) {
        create_local(&pool, "blobcat", "1.png", "image/png", 0, None)
            .await
            .unwrap()
            .unwrap();
        upsert_remote(&pool, remote("https://remote.example/a.png", None))
            .await
            .unwrap();

        let codes = vec!["blobcat".to_owned(), "missing".to_owned()];
        let local = lookup(&pool, &codes, None).await.unwrap();
        assert_eq!(local.len(), 1);
        assert!(local[0].domain.is_none());
        assert_eq!(local[0].image_file_name.as_deref(), Some("1.png"));

        let other = lookup(&pool, &codes, Some("other.example")).await.unwrap();
        assert!(other.is_empty(), "no cross-domain leakage");

        sqlx::query!("UPDATE custom_emojis SET disabled = TRUE WHERE domain IS NULL")
            .execute(&pool)
            .await
            .unwrap();
        assert!(lookup(&pool, &codes, None).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn lookup_preserves_shortcode_order(pool: PgPool) {
        for code in ["zzz", "aaa", "mmm"] {
            create_local(&pool, code, "1.png", "image/png", 0, None)
                .await
                .unwrap()
                .unwrap();
        }
        let codes = vec!["zzz".to_owned(), "mmm".to_owned(), "aaa".to_owned()];
        let found = lookup(&pool, &codes, None).await.unwrap();
        let order: Vec<&str> = found.iter().map(|e| e.shortcode.as_str()).collect();
        assert_eq!(order, ["zzz", "mmm", "aaa"]);
    }

    #[sqlx::test]
    async fn listed_serves_only_picker_visible_local_emoji(pool: PgPool) {
        create_local(&pool, "visible", "1.png", "image/png", 0, None)
            .await
            .unwrap()
            .unwrap();
        let hidden = create_local(&pool, "hidden", "2.png", "image/png", 0, None)
            .await
            .unwrap()
            .unwrap();
        sqlx::query!(
            "UPDATE custom_emojis SET visible_in_picker = FALSE WHERE id = $1",
            hidden.id
        )
        .execute(&pool)
        .await
        .unwrap();
        upsert_remote(&pool, remote("https://remote.example/a.png", None))
            .await
            .unwrap();

        let picker = listed(&pool).await.unwrap();
        let codes: Vec<&str> = picker.iter().map(|e| e.shortcode.as_str()).collect();
        assert_eq!(codes, ["visible"]);

        // The CLI listing still shows the hidden one.
        let all = list_local(&pool).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[sqlx::test]
    async fn create_local_rejects_duplicates_and_delete_removes(pool: PgPool) {
        let created = create_local(&pool, "blobcat", "1.png", "image/png", 0, None)
            .await
            .unwrap();
        assert!(created.is_some());
        let duplicate = create_local(&pool, "blobcat", "2.png", "image/png", 0, None)
            .await
            .unwrap();
        assert!(duplicate.is_none());

        let found = find_local_by_id(&pool, created.unwrap().id).await.unwrap();
        assert_eq!(found.unwrap().shortcode, "blobcat");

        assert!(delete_local(&pool, "blobcat").await.unwrap());
        assert!(!delete_local(&pool, "blobcat").await.unwrap());
    }

    #[sqlx::test]
    async fn update_flags_and_delete_by_id_are_local_only(pool: PgPool) {
        let local = create_local(&pool, "party", "1.png", "image/png", 123, None)
            .await
            .unwrap()
            .unwrap();
        upsert_remote(&pool, remote("https://remote.example/a.png", None))
            .await
            .unwrap();
        let remote = lookup(&pool, &["blobcat".to_owned()], Some("remote.example"))
            .await
            .unwrap()
            .pop()
            .unwrap();

        let updated = update_local_flags(&pool, local.id, true, false, Some("reactions"))
            .await
            .unwrap()
            .unwrap();
        assert!(updated.disabled);
        assert!(!updated.visible_in_picker);
        assert_eq!(updated.category.as_deref(), Some("reactions"));
        let cleared = update_local_flags(&pool, local.id, true, false, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cleared.category, None);
        assert!(
            update_local_flags(&pool, remote.id, true, false, None)
                .await
                .unwrap()
                .is_none()
        );

        let deleted = delete_local_by_id(&pool, local.id).await.unwrap().unwrap();
        assert_eq!(deleted.image_file_name.as_deref(), Some("1.png"));
        assert!(
            delete_local_by_id(&pool, remote.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(list_local(&pool).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn personal_quota_is_atomic_and_retired_rows_stop_counting(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        set_personal_limit(&pool, 1).await.unwrap();
        let first =
            create_personal_upload(&pool, alice, "first", "first.png", "image/png", 10, None)
                .await
                .unwrap();
        let PersonalCreateOutcome::Created(first_id) = first else {
            panic!()
        };
        assert_eq!(
            create_personal_upload(&pool, alice, "second", "second.png", "image/png", 10, None)
                .await
                .unwrap(),
            PersonalCreateOutcome::LimitReached
        );
        assert!(retire_personal(&pool, alice, first_id).await.unwrap());
        assert!(matches!(
            create_personal_upload(&pool, alice, "second", "second.png", "image/png", 10, None)
                .await
                .unwrap(),
            PersonalCreateOutcome::Created(_)
        ));
    }

    #[sqlx::test]
    async fn borrowing_preserves_origin_and_cannot_duplicate_it(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        upsert_remote(&pool, remote("https://remote.example/a.png", None))
            .await
            .unwrap();
        let source = lookup(&pool, &["blobcat".into()], Some("remote.example"))
            .await
            .unwrap()
            .pop()
            .unwrap();
        let source = find_managed_by_id(&pool, source.id).await.unwrap().unwrap();
        let created = create_personal_borrow(&pool, alice, &source, "copy.png", "image/png", 12)
            .await
            .unwrap();
        let PersonalCreateOutcome::Created(copy_id) = created else {
            panic!()
        };
        let copy = find_managed_by_id(&pool, copy_id).await.unwrap().unwrap();
        assert!(copy.borrowed);
        assert_eq!(copy.origin_id, source.origin_id);
        assert_eq!(
            create_personal_borrow(&pool, alice, &source, "again.png", "image/png", 12)
                .await
                .unwrap(),
            PersonalCreateOutcome::OriginAlreadyOwned
        );
    }

    #[sqlx::test]
    async fn personal_namespace_shadows_global_and_lists_first(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        create_local(&pool, "party", "global.png", "image/png", 10, None)
            .await
            .unwrap();
        create_local(&pool, "wave", "wave.png", "image/png", 10, None)
            .await
            .unwrap();
        create_personal_upload(
            &pool,
            alice,
            "party",
            "mine.png",
            "image/png",
            10,
            Some("Mine"),
        )
        .await
        .unwrap();
        let found = lookup_local_for_account(&pool, alice, &["party".into(), "wave".into()])
            .await
            .unwrap();
        assert_eq!(found[0].image_file_name.as_deref(), Some("mine.png"));
        let picker = listed_for_account(&pool, alice).await.unwrap();
        assert!(picker[0].1);
        assert_eq!(
            picker
                .iter()
                .filter(|(emoji, _)| emoji.shortcode == "party")
                .count(),
            1
        );
    }

    #[sqlx::test]
    async fn promotion_retires_equivalent_personal_copies(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        upsert_remote(&pool, remote("https://remote.example/a.png", None))
            .await
            .unwrap();
        let source = lookup(&pool, &["blobcat".into()], Some("remote.example"))
            .await
            .unwrap()
            .pop()
            .unwrap();
        let source = find_managed_by_id(&pool, source.id).await.unwrap().unwrap();
        let PersonalCreateOutcome::Created(alice_copy) =
            create_personal_borrow(&pool, alice, &source, "a.png", "image/png", 1)
                .await
                .unwrap()
        else {
            panic!()
        };
        create_personal_borrow(&pool, bob, &source, "b.png", "image/png", 1)
            .await
            .unwrap();
        assert_eq!(
            promote_personal(&pool, alice_copy, "blobcat", Some("Blobs"))
                .await
                .unwrap(),
            PromoteOutcome::Promoted(alice_copy)
        );
        assert!(list_personal(&pool, alice).await.unwrap().is_empty());
        assert!(list_personal(&pool, bob).await.unwrap().is_empty());
        let global = list_local(&pool).await.unwrap();
        assert_eq!(global.len(), 1);
        assert_eq!(global[0].shortcode, "blobcat");
        assert_eq!(global[0].category.as_deref(), Some("Blobs"));
    }

    #[sqlx::test]
    async fn trends_dedupe_copies_by_origin_and_rank_unique_users(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        upsert_remote(&pool, remote("https://remote.example/a.png", None))
            .await
            .unwrap();
        let source = lookup(&pool, &["blobcat".into()], Some("remote.example"))
            .await
            .unwrap()
            .pop()
            .unwrap();
        let source = find_managed_by_id(&pool, source.id).await.unwrap().unwrap();
        create_personal_borrow(&pool, alice, &source, "a.png", "image/png", 1)
            .await
            .unwrap();
        record_post_usage(&pool, alice, &[source.origin_id])
            .await
            .unwrap();
        record_reaction_usage(&pool, alice, source.origin_id)
            .await
            .unwrap();
        record_post_usage(&pool, bob, &[source.origin_id])
            .await
            .unwrap();
        let rows = trending(&pool, TrendingScope::Federated, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].unique_users, 2);
        assert_eq!(rows[0].total_uses, 3);
    }

    #[sqlx::test]
    async fn rename_keeps_historical_alias_out_of_new_composition(pool: PgPool) {
        let alice = local(&pool, "alias_alice").await;
        let PersonalCreateOutcome::Created(emoji_id) =
            create_personal_upload(&pool, alice, "old_name", "mine.png", "image/png", 1, None)
                .await
                .unwrap()
        else {
            panic!()
        };
        update_personal(&pool, alice, emoji_id, "new_name", None)
            .await
            .unwrap()
            .unwrap();
        assert!(
            lookup_local_for_account(&pool, alice, &["old_name".into()])
                .await
                .unwrap()
                .is_empty()
        );
        let historical =
            lookup_many_for_authors(&pool, &["old_name".into()], &[None], &[Some(alice)])
                .await
                .unwrap();
        assert_eq!(historical.len(), 1);
        assert_eq!(historical[0].shortcode, "old_name");
        assert_eq!(historical[0].image_file_name.as_deref(), Some("mine.png"));
    }

    #[sqlx::test]
    async fn owner_cannot_edit_a_borrowed_copy(pool: PgPool) {
        let alice = local(&pool, "immutable_alice").await;
        upsert_remote(&pool, remote("https://remote.example/a.png", None))
            .await
            .unwrap();
        let source = lookup(&pool, &["blobcat".into()], Some("remote.example"))
            .await
            .unwrap()
            .pop()
            .unwrap();
        let source = find_managed_by_id(&pool, source.id).await.unwrap().unwrap();
        let PersonalCreateOutcome::Created(copy_id) =
            create_personal_borrow(&pool, alice, &source, "copy.png", "image/png", 1)
                .await
                .unwrap()
        else {
            panic!()
        };
        assert!(
            update_personal(&pool, alice, copy_id, "renamed", Some("Mine"))
                .await
                .unwrap()
                .is_none()
        );
    }
}
