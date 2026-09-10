//! Content filters (Mastodon's `custom_filters`): a per-account named filter
//! with an action and the contexts it applies in, matching keyword phrases or
//! specific statuses. The matching itself (regex compilation, searchable-text
//! extraction) lives in the server crate; this module owns storage and the
//! unexpired-filters fetch the application path reads.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// The contexts a filter may apply in (Mastodon's `VALID_CONTEXTS`).
pub const VALID_CONTEXTS: [&str; 5] = ["home", "notifications", "public", "thread", "account"];
/// The filter actions Mastodon's `action` enum admits.
pub const ACTIONS: [&str; 3] = ["warn", "hide", "blur"];
/// Mastodon's `CustomFilter::TITLE_LENGTH_LIMIT` (characters).
pub const TITLE_LENGTH_LIMIT: usize = 256;
/// Mastodon's `CustomFilterKeyword::KEYWORD_LENGTH_LIMIT` (characters).
pub const KEYWORD_LENGTH_LIMIT: usize = 512;
/// Cardinality guard (audit #58): the greatest number of filters a single
/// account may own. Mastodon imposes no ceiling; this one is far above honest
/// use but stops an account from persisting unbounded durable filters, every
/// one of which every signed-in render must load and compile.
pub const MAX_FILTERS_PER_ACCOUNT: usize = 100;
/// Cardinality guard (audit #58): the greatest number of keywords a single
/// filter may carry. Bounds both the durable keyword rows and the regex union
/// compiled from them on every render (aggregate pattern bytes are then bounded
/// by this times [`KEYWORD_LENGTH_LIMIT`]).
pub const MAX_KEYWORDS_PER_FILTER: usize = 100;

#[derive(Debug, Clone)]
pub struct CustomFilter {
    pub id: i64,
    pub account_id: i64,
    pub title: String,
    pub action: String,
    pub context: Vec<String>,
    pub expires_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone)]
pub struct CustomFilterKeyword {
    pub id: i64,
    pub custom_filter_id: i64,
    pub keyword: String,
    pub whole_word: bool,
}

#[derive(Debug, Clone)]
pub struct CustomFilterStatus {
    pub id: i64,
    pub custom_filter_id: i64,
    pub status_id: i64,
}

/// A keyword to create alongside a filter (the validated `keywords_attributes`
/// of a create, or the body of a standalone keyword create).
#[derive(Debug, Clone)]
pub struct NewKeyword {
    pub keyword: String,
    pub whole_word: bool,
}

/// One entry of a v2 update's `keywords_attributes`: create, partial update or
/// destroy, like Rails' nested attributes.
#[derive(Debug, Clone)]
pub enum KeywordChange {
    Create {
        keyword: String,
        whole_word: bool,
    },
    Update {
        id: i64,
        keyword: Option<String>,
        whole_word: Option<bool>,
    },
    Destroy {
        id: i64,
    },
}

/// Creates a filter and its initial keywords transactionally. Caller validates.
pub async fn create(
    pool: &PgPool,
    account_id: i64,
    title: &str,
    action: &str,
    context: &[String],
    expires_at: Option<OffsetDateTime>,
    keywords: &[NewKeyword],
) -> Result<CustomFilter, DbError> {
    let mut tx = pool.begin().await?;
    let filter = sqlx::query_as!(
        CustomFilter,
        r#"
        INSERT INTO custom_filters (id, account_id, title, action, context, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, account_id, title, action, context, expires_at
        "#,
        id::next(),
        account_id,
        title,
        action,
        context,
        expires_at,
    )
    .fetch_one(&mut *tx)
    .await?;
    // One set-based insert rather than a statement per keyword.
    if !keywords.is_empty() {
        let ids: Vec<i64> = keywords.iter().map(|_| id::next()).collect();
        let texts: Vec<&str> = keywords.iter().map(|k| k.keyword.as_str()).collect();
        let whole: Vec<bool> = keywords.iter().map(|k| k.whole_word).collect();
        sqlx::query!(
            r#"
            INSERT INTO custom_filter_keywords (id, custom_filter_id, keyword, whole_word)
            SELECT u.id, $2, u.keyword, u.whole_word
            FROM unnest($1::bigint[], $3::text[], $4::bool[]) AS u(id, keyword, whole_word)
            "#,
            &ids,
            filter.id,
            &texts as &[&str],
            &whole,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(filter)
}

/// A filter by id, only if `account_id` owns it — every filter endpoint scopes
/// like this (`current_account.custom_filters.find`).
pub async fn find_owned(
    pool: &PgPool,
    account_id: i64,
    filter_id: i64,
) -> Result<Option<CustomFilter>, DbError> {
    let filter = sqlx::query_as!(
        CustomFilter,
        r#"SELECT id, account_id, title, action, context, expires_at
           FROM custom_filters WHERE id = $1 AND account_id = $2"#,
        filter_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(filter)
}

/// How many filters `account_id` owns — the per-account cardinality guard's
/// admission check ([`MAX_FILTERS_PER_ACCOUNT`]).
pub async fn count_owned(pool: &PgPool, account_id: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM custom_filters WHERE account_id = $1"#,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Every filter `account_id` owns, oldest first.
pub async fn owned_by(pool: &PgPool, account_id: i64) -> Result<Vec<CustomFilter>, DbError> {
    let filters = sqlx::query_as!(
        CustomFilter,
        r#"SELECT id, account_id, title, action, context, expires_at
           FROM custom_filters WHERE account_id = $1 ORDER BY id"#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(filters)
}

/// Updates a filter's own attributes and applies any keyword changes
/// transactionally (the handler merges absent attributes first).
pub async fn update(
    pool: &PgPool,
    filter_id: i64,
    title: &str,
    action: &str,
    context: &[String],
    expires_at: Option<OffsetDateTime>,
    keyword_changes: &[KeywordChange],
) -> Result<CustomFilter, DbError> {
    let mut tx = pool.begin().await?;
    let filter = sqlx::query_as!(
        CustomFilter,
        r#"
        UPDATE custom_filters
        SET title = $2, action = $3, context = $4, expires_at = $5, updated_at = now()
        WHERE id = $1
        RETURNING id, account_id, title, action, context, expires_at
        "#,
        filter_id,
        title,
        action,
        context,
        expires_at,
    )
    .fetch_one(&mut *tx)
    .await?;
    // Collect the like-kinded changes so creates, updates and destroys each
    // run as one set-based statement rather than a query per keyword. Partial
    // in-place edits fold NULLs through unnest so each row still
    // COALESCEs its own column subset; repeated edits of one keyword compose
    // in order, as the per-row statements did.
    let mut create_ids = Vec::new();
    let mut create_texts: Vec<&str> = Vec::new();
    let mut create_whole = Vec::new();
    let mut update_ids: Vec<i64> = Vec::new();
    let mut update_keywords: Vec<Option<&str>> = Vec::new();
    let mut update_whole: Vec<Option<bool>> = Vec::new();
    let mut destroy_ids = Vec::new();
    for change in keyword_changes {
        match change {
            KeywordChange::Create {
                keyword,
                whole_word,
            } => {
                create_ids.push(id::next());
                create_texts.push(keyword.as_str());
                create_whole.push(*whole_word);
            }
            KeywordChange::Update {
                id: keyword_id,
                keyword,
                whole_word,
            } => {
                if let Some(slot) = update_ids.iter().position(|id| id == keyword_id) {
                    if let Some(text) = keyword.as_deref() {
                        update_keywords[slot] = Some(text);
                    }
                    if let Some(whole) = *whole_word {
                        update_whole[slot] = Some(whole);
                    }
                } else {
                    update_ids.push(*keyword_id);
                    update_keywords.push(keyword.as_deref());
                    update_whole.push(*whole_word);
                }
            }
            KeywordChange::Destroy { id: keyword_id } => destroy_ids.push(*keyword_id),
        }
    }
    apply_keyword_edits(
        &mut tx,
        filter_id,
        &update_ids,
        &update_keywords,
        &update_whole,
    )
    .await?;
    if !create_ids.is_empty() {
        sqlx::query!(
            r#"
            INSERT INTO custom_filter_keywords (id, custom_filter_id, keyword, whole_word)
            SELECT u.id, $2, u.keyword, u.whole_word
            FROM unnest($1::bigint[], $3::text[], $4::bool[]) AS u(id, keyword, whole_word)
            "#,
            &create_ids,
            filter_id,
            &create_texts as &[&str],
            &create_whole,
        )
        .execute(&mut *tx)
        .await?;
    }
    if !destroy_ids.is_empty() {
        sqlx::query!(
            "DELETE FROM custom_filter_keywords
             WHERE custom_filter_id = $1 AND id = ANY($2)",
            filter_id,
            &destroy_ids,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(filter)
}

/// Applies the in-place keyword edits of one filter update as a single
/// statement; NULLs in the unnest keep the stored column, exactly as the
/// per-row COALESCE updates did.
async fn apply_keyword_edits(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    filter_id: i64,
    update_ids: &[i64],
    update_keywords: &[Option<&str>],
    update_whole: &[Option<bool>],
) -> Result<(), DbError> {
    if update_ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        r#"
        UPDATE custom_filter_keywords k
        SET keyword = COALESCE(v.keyword, k.keyword),
            whole_word = COALESCE(v.whole_word, k.whole_word),
            updated_at = now()
        FROM unnest($2::bigint[], $3::text[], $4::bool[]) AS v(id, keyword, whole_word)
        WHERE k.id = v.id AND k.custom_filter_id = $1
        "#,
        filter_id,
        update_ids,
        update_keywords as &[Option<&str>],
        update_whole as &[Option<bool>],
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Deletes an owned filter (keywords and status entries cascade); returns
/// whether a row matched.
pub async fn delete(pool: &PgPool, account_id: i64, filter_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM custom_filters WHERE id = $1 AND account_id = $2",
        filter_id,
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

// --- Keywords --------------------------------------------------------------

/// The keywords of a single filter, oldest first.
pub async fn keywords_for(
    pool: &PgPool,
    filter_id: i64,
) -> Result<Vec<CustomFilterKeyword>, DbError> {
    let keywords = sqlx::query_as!(
        CustomFilterKeyword,
        r#"SELECT id, custom_filter_id, keyword, whole_word
           FROM custom_filter_keywords WHERE custom_filter_id = $1 ORDER BY id"#,
        filter_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(keywords)
}

/// The keywords of a batch of filters, oldest first — for serializing a list
/// of filters with their rules.
pub async fn keywords_for_filters(
    pool: &PgPool,
    filter_ids: &[i64],
) -> Result<Vec<CustomFilterKeyword>, DbError> {
    let keywords = sqlx::query_as!(
        CustomFilterKeyword,
        r#"SELECT id, custom_filter_id, keyword, whole_word
           FROM custom_filter_keywords WHERE custom_filter_id = ANY($1) ORDER BY id"#,
        filter_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(keywords)
}

/// A keyword by id, only if the owning filter belongs to `account_id`
/// (`CustomFilterKeyword.where(custom_filter: { account: current })`).
pub async fn find_owned_keyword(
    pool: &PgPool,
    account_id: i64,
    keyword_id: i64,
) -> Result<Option<CustomFilterKeyword>, DbError> {
    let keyword = sqlx::query_as!(
        CustomFilterKeyword,
        r#"
        SELECT k.id, k.custom_filter_id, k.keyword, k.whole_word
        FROM custom_filter_keywords k
        JOIN custom_filters f ON f.id = k.custom_filter_id
        WHERE k.id = $1 AND f.account_id = $2
        "#,
        keyword_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(keyword)
}

/// Adds a keyword to a filter. Caller validates and checks filter ownership.
pub async fn create_keyword(
    pool: &PgPool,
    filter_id: i64,
    keyword: &str,
    whole_word: bool,
) -> Result<CustomFilterKeyword, DbError> {
    let row = sqlx::query_as!(
        CustomFilterKeyword,
        r#"
        INSERT INTO custom_filter_keywords (id, custom_filter_id, keyword, whole_word)
        VALUES ($1, $2, $3, $4)
        RETURNING id, custom_filter_id, keyword, whole_word
        "#,
        id::next(),
        filter_id,
        keyword,
        whole_word,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Updates a keyword's fields (the handler merges absent attributes first).
pub async fn update_keyword(
    pool: &PgPool,
    keyword_id: i64,
    keyword: &str,
    whole_word: bool,
) -> Result<CustomFilterKeyword, DbError> {
    let row = sqlx::query_as!(
        CustomFilterKeyword,
        r#"
        UPDATE custom_filter_keywords
        SET keyword = $2, whole_word = $3, updated_at = now()
        WHERE id = $1
        RETURNING id, custom_filter_id, keyword, whole_word
        "#,
        keyword_id,
        keyword,
        whole_word,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Deletes a keyword by id (ownership already checked); returns whether it
/// existed.
pub async fn delete_keyword(pool: &PgPool, keyword_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM custom_filter_keywords WHERE id = $1",
        keyword_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// How many keywords a filter has — the v1 update's
/// `deprecated_api_multiple_keywords` guard.
pub async fn count_keywords(pool: &PgPool, filter_id: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM custom_filter_keywords WHERE custom_filter_id = $1"#,
        filter_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// [`count_keywords`] over a set of filters in one grouped query, keyed by
/// filter id. Filters with no keywords are absent (count as zero).
pub async fn count_keywords_many(
    pool: &PgPool,
    filter_ids: &[i64],
) -> Result<std::collections::HashMap<i64, i64>, DbError> {
    if filter_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows = sqlx::query!(
        r#"
        SELECT custom_filter_id, count(*) AS "count!"
        FROM custom_filter_keywords
        WHERE custom_filter_id = ANY($1)
        GROUP BY custom_filter_id
        "#,
        filter_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.custom_filter_id, row.count))
        .collect())
}

// --- Status entries --------------------------------------------------------

/// The status entries of a single filter, oldest first.
pub async fn statuses_for(
    pool: &PgPool,
    filter_id: i64,
) -> Result<Vec<CustomFilterStatus>, DbError> {
    let rows = sqlx::query_as!(
        CustomFilterStatus,
        r#"SELECT id, custom_filter_id, status_id
           FROM custom_filter_statuses WHERE custom_filter_id = $1 ORDER BY id"#,
        filter_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// A status entry by id, only if the owning filter belongs to `account_id`.
pub async fn find_owned_status(
    pool: &PgPool,
    account_id: i64,
    entry_id: i64,
) -> Result<Option<CustomFilterStatus>, DbError> {
    let row = sqlx::query_as!(
        CustomFilterStatus,
        r#"
        SELECT s.id, s.custom_filter_id, s.status_id
        FROM custom_filter_statuses s
        JOIN custom_filters f ON f.id = s.custom_filter_id
        WHERE s.id = $1 AND f.account_id = $2
        "#,
        entry_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Adds a status to a filter. Returns `None` when it was already there
/// (Mastodon's `status_id` uniqueness — "Status has already been taken").
pub async fn create_status(
    pool: &PgPool,
    filter_id: i64,
    status_id: i64,
) -> Result<Option<CustomFilterStatus>, DbError> {
    let row = sqlx::query_as!(
        CustomFilterStatus,
        r#"
        INSERT INTO custom_filter_statuses (id, custom_filter_id, status_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (custom_filter_id, status_id) DO NOTHING
        RETURNING id, custom_filter_id, status_id
        "#,
        id::next(),
        filter_id,
        status_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Deletes a status entry by id (ownership already checked).
pub async fn delete_status(pool: &PgPool, entry_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!("DELETE FROM custom_filter_statuses WHERE id = $1", entry_id,)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

// --- Application path -------------------------------------------------------

/// A filter and its match rules, for the `filtered` attribute. `status_ids` is
/// the filter's `custom_filter_statuses`; keywords come back as raw rows the
/// server compiles into a regex union.
#[derive(Debug, Clone)]
pub struct ActiveFilter {
    pub filter: CustomFilter,
    pub keywords: Vec<CustomFilterKeyword>,
    pub status_ids: Vec<i64>,
}

/// Every unexpired filter `account_id` owns, with its keywords and pinned
/// status ids — Mastodon's `cached_filters_for`. Returned regardless of
/// context; the client applies each filter's `context`.
pub async fn active_for(pool: &PgPool, account_id: i64) -> Result<Vec<ActiveFilter>, DbError> {
    let filters = sqlx::query_as!(
        CustomFilter,
        r#"SELECT id, account_id, title, action, context, expires_at
           FROM custom_filters
           WHERE account_id = $1 AND (expires_at IS NULL OR expires_at > now())
           ORDER BY id"#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    if filters.is_empty() {
        return Ok(Vec::new());
    }
    let filter_ids: Vec<i64> = filters.iter().map(|f| f.id).collect();
    let keywords = keywords_for_filters(pool, &filter_ids).await?;
    let status_rows = sqlx::query!(
        r#"SELECT custom_filter_id, status_id
           FROM custom_filter_statuses WHERE custom_filter_id = ANY($1)"#,
        &filter_ids,
    )
    .fetch_all(pool)
    .await?;
    // Group both dependent sets by filter id in one pass each, rather than
    // rescanning the full keyword/status vectors once per filter.
    let mut keywords_by_filter: std::collections::HashMap<i64, Vec<CustomFilterKeyword>> =
        std::collections::HashMap::new();
    for keyword in keywords {
        keywords_by_filter
            .entry(keyword.custom_filter_id)
            .or_default()
            .push(keyword);
    }
    let mut statuses_by_filter: std::collections::HashMap<i64, Vec<i64>> =
        std::collections::HashMap::new();
    for row in status_rows {
        statuses_by_filter
            .entry(row.custom_filter_id)
            .or_default()
            .push(row.status_id);
    }
    let active = filters
        .into_iter()
        .map(|filter| ActiveFilter {
            keywords: keywords_by_filter.remove(&filter.id).unwrap_or_default(),
            status_ids: statuses_by_filter.remove(&filter.id).unwrap_or_default(),
            filter,
        })
        .collect();
    Ok(active)
}

/// [`active_for`] across a set of accounts in one pass, keyed by account id —
/// the streaming fan-out's filter loader for the recipients whose compiled set
/// is not cached. Each account's filters come back in the singular form's
/// order (`ORDER BY id`); accounts with no unexpired filter are absent.
pub async fn active_for_many(
    pool: &PgPool,
    account_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<ActiveFilter>>, DbError> {
    let filters = sqlx::query_as!(
        CustomFilter,
        r#"SELECT id, account_id, title, action, context, expires_at
           FROM custom_filters
           WHERE account_id = ANY($1) AND (expires_at IS NULL OR expires_at > now())
           ORDER BY id"#,
        account_ids,
    )
    .fetch_all(pool)
    .await?;
    if filters.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let filter_ids: Vec<i64> = filters.iter().map(|f| f.id).collect();
    let keywords = keywords_for_filters(pool, &filter_ids).await?;
    let status_rows = sqlx::query!(
        r#"SELECT custom_filter_id, status_id
           FROM custom_filter_statuses WHERE custom_filter_id = ANY($1)"#,
        &filter_ids,
    )
    .fetch_all(pool)
    .await?;
    let mut keywords_by_filter: std::collections::HashMap<i64, Vec<CustomFilterKeyword>> =
        std::collections::HashMap::new();
    for keyword in keywords {
        keywords_by_filter
            .entry(keyword.custom_filter_id)
            .or_default()
            .push(keyword);
    }
    let mut statuses_by_filter: std::collections::HashMap<i64, Vec<i64>> =
        std::collections::HashMap::new();
    for row in status_rows {
        statuses_by_filter
            .entry(row.custom_filter_id)
            .or_default()
            .push(row.status_id);
    }
    let mut by_account: std::collections::HashMap<i64, Vec<ActiveFilter>> =
        std::collections::HashMap::new();
    for filter in filters {
        by_account
            .entry(filter.account_id)
            .or_default()
            .push(ActiveFilter {
                keywords: keywords_by_filter.remove(&filter.id).unwrap_or_default(),
                status_ids: statuses_by_filter.remove(&filter.id).unwrap_or_default(),
                filter,
            });
    }
    Ok(by_account)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::status::{self, NewLocalStatus};

    async fn local_account(pool: &PgPool, username: &str) -> i64 {
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

    fn ctx(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    #[sqlx::test]
    async fn filter_crud_with_keywords(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;

        let filter = create(
            &pool,
            alice,
            "Spoilers",
            "warn",
            &ctx(&["home", "public"]),
            None,
            &[NewKeyword {
                keyword: "godzilla".to_owned(),
                whole_word: true,
            }],
        )
        .await
        .unwrap();
        assert_eq!(filter.context, ctx(&["home", "public"]));
        assert_eq!(owned_by(&pool, alice).await.unwrap().len(), 1);
        // Owner-scoped: bob can't see alice's filter.
        assert!(find_owned(&pool, bob, filter.id).await.unwrap().is_none());
        assert_eq!(keywords_for(&pool, filter.id).await.unwrap().len(), 1);

        // Update merges and applies keyword changes.
        let existing = keywords_for(&pool, filter.id).await.unwrap();
        let updated = update(
            &pool,
            filter.id,
            "Monsters",
            "hide",
            &ctx(&["home"]),
            None,
            &[
                KeywordChange::Update {
                    id: existing[0].id,
                    keyword: Some("gojira".to_owned()),
                    whole_word: None,
                },
                KeywordChange::Create {
                    keyword: "mothra".to_owned(),
                    whole_word: false,
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!(updated.title, "Monsters");
        assert_eq!(updated.action, "hide");
        let keywords = keywords_for(&pool, filter.id).await.unwrap();
        assert_eq!(keywords.len(), 2);
        assert_eq!(keywords[0].keyword, "gojira");
        assert!(keywords[0].whole_word, "absent whole_word kept");

        // Destroy a keyword via the nested change.
        update(
            &pool,
            filter.id,
            "Monsters",
            "hide",
            &ctx(&["home"]),
            None,
            &[KeywordChange::Destroy { id: keywords[1].id }],
        )
        .await
        .unwrap();
        assert_eq!(count_keywords(&pool, filter.id).await.unwrap(), 1);

        assert!(delete(&pool, alice, filter.id).await.unwrap());
        assert!(!delete(&pool, alice, filter.id).await.unwrap());
    }

    #[sqlx::test]
    async fn count_owned_is_per_account(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        assert_eq!(count_owned(&pool, alice).await.unwrap(), 0);
        for title in ["a", "b", "c"] {
            create(&pool, alice, title, "warn", &ctx(&["home"]), None, &[])
                .await
                .unwrap();
        }
        create(&pool, bob, "b", "warn", &ctx(&["home"]), None, &[])
            .await
            .unwrap();
        assert_eq!(count_owned(&pool, alice).await.unwrap(), 3);
        assert_eq!(count_owned(&pool, bob).await.unwrap(), 1);
    }

    #[sqlx::test]
    async fn status_entries_unique_and_owner_scoped(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let bob = local_account(&pool, "bob").await;
        let post = status::create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>hi</p>", "public", None),
        )
        .await
        .unwrap()
        .id;

        let filter = create(&pool, alice, "f", "warn", &ctx(&["home"]), None, &[])
            .await
            .unwrap();
        let entry = create_status(&pool, filter.id, post)
            .await
            .unwrap()
            .expect("first add");
        // Duplicate is rejected (None → "already been taken").
        assert!(
            create_status(&pool, filter.id, post)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(statuses_for(&pool, filter.id).await.unwrap().len(), 1);
        assert!(
            find_owned_status(&pool, bob, entry.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            find_owned_status(&pool, alice, entry.id)
                .await
                .unwrap()
                .is_some()
        );

        assert!(delete_status(&pool, entry.id).await.unwrap());
        assert!(statuses_for(&pool, filter.id).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn active_for_skips_expired_and_bundles_rules(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let post = status::create_local(
            &pool,
            NewLocalStatus::new(alice, "<p>hi</p>", "public", None),
        )
        .await
        .unwrap()
        .id;

        let live = create(
            &pool,
            alice,
            "live",
            "warn",
            &ctx(&["home"]),
            None,
            &[NewKeyword {
                keyword: "spoiler".to_owned(),
                whole_word: true,
            }],
        )
        .await
        .unwrap();
        create_status(&pool, live.id, post).await.unwrap();
        // An expired filter is never returned.
        let past = OffsetDateTime::now_utc() - time::Duration::hours(1);
        create(
            &pool,
            alice,
            "old",
            "hide",
            &ctx(&["home"]),
            Some(past),
            &[],
        )
        .await
        .unwrap();

        let active = active_for(&pool, alice).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].filter.id, live.id);
        assert_eq!(active[0].keywords.len(), 1);
        assert_eq!(active[0].status_ids, [post]);
    }
}
