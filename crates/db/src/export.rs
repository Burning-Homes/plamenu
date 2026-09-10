//! Read queries backing the CSV data-export endpoints (Mastodon's
//! `Settings::Exports`).
//!
//! Each query returns rows in the order Mastodon emits them (newest first,
//! keyed on the relationship row id) so the exported files line up with what a
//! Mastodon instance would produce. Address rendering is left to the caller:
//! these rows carry `username` + an optional remote `domain` (`None` for owned
//! local and portable local-namespace accounts), and the web layer stitches
//! on `{local_domain}` where needed.
//!
//! Every read is keyset-paged: `after` is the previous page's last cursor
//! (`None` starts at the newest row) and `limit` bounds the page. The
//! synchronous CSV download endpoints stream page-sized chunks so one request
//! never materialises an unbounded result vector or CSV body and large
//! accounts are never truncated; the account-archive and
//! import-reconciliation callers, which must observe the *complete* set in
//! one vector, pass `None` + [`UNLIMITED`].

use sqlx::PgPool;

use crate::DbError;

/// The `limit` sentinel for callers that must read the *complete* relationship
/// set rather than a bounded export page: the account archive
/// (`likes.json`/`bookmarks.json`) and import reconciliation (which diffs the
/// full current set to decide what to remove). No account approaches this many
/// rows, so it is effectively unbounded.
pub const UNLIMITED: i64 = i64::MAX;

/// One user-level domain block, with the `account_domain_blocks.id` that
/// keyset-pages the dataset.
#[derive(Debug)]
pub struct BlockedDomainRow {
    pub cursor: i64,
    pub domain: String,
}

/// An account reference for the address-only CSVs. `domain` is `None` for
/// local accounts (rendered as `username@{local_domain}`).
#[derive(Debug)]
pub struct AcctRow {
    /// The relationship-row id that keyset-pages this dataset.
    pub cursor: i64,
    pub username: String,
    pub domain: Option<String>,
}

/// A muted account plus its `hide_notifications` flag.
#[derive(Debug)]
pub struct MutedRow {
    /// The `mutes.id` that keyset-pages this dataset.
    pub cursor: i64,
    pub username: String,
    pub domain: Option<String>,
    pub hide_notifications: bool,
}

/// A saved-status reference — a bookmark ([`bookmarks`]) or favourite
/// ([`favourites_page`]). `uri` is the stored `ActivityPub` id for remote
/// statuses and `None` for local ones (derived by the caller from
/// `author_username` + `status_id`).
#[derive(Debug)]
pub struct BookmarkRow {
    /// The `bookmarks.id` that keyset-pages this dataset.
    pub cursor: i64,
    pub uri: Option<String>,
    pub author_username: String,
    pub author_uri: Option<String>,
    pub status_id: i64,
}

/// A saved-status reference plus the relationship-row id that keyset-pages it.
/// The account archive streams its `likes.json`/`bookmarks.json` collections
/// with [`favourites_page`]/[`bookmarks_page`] instead of reading the whole set
/// into memory, so `cursor` (the `favourites.id`/`bookmarks.id`
/// of the row) drives the next page.
#[derive(Debug)]
pub struct ExportRef {
    pub cursor: i64,
    pub uri: Option<String>,
    pub author_username: String,
    pub author_uri: Option<String>,
    pub status_id: i64,
}

/// One list membership: the owning list's title and the member account.
#[derive(Debug)]
pub struct ListMembershipRow {
    /// The `(lists.id, list_accounts.id)` pair that keyset-pages this dataset
    /// (it is ordered ascending by list, then member).
    pub list_cursor: i64,
    pub member_cursor: i64,
    pub title: String,
    pub username: String,
    pub domain: Option<String>,
}

/// A followed account plus the per-follow settings columns of the
/// `following_accounts.csv` export (M32).
#[derive(Debug)]
pub struct FollowingRow {
    /// The `follows.id` that keyset-pages this dataset.
    pub cursor: i64,
    pub username: String,
    pub domain: Option<String>,
    pub show_reblogs: bool,
    pub notify: bool,
    pub languages: Option<Vec<String>>,
    /// The per-follow reply switch — our own trailing column, after
    /// Mastodon's.
    pub with_replies: bool,
}

/// Accounts `account_id` actively follows (accepted, non-pending), newest
/// first — Mastodon's `active_relationships.reorder(id: :desc)`.
pub async fn following(
    pool: &PgPool,
    account_id: i64,
    after: Option<i64>,
    limit: i64,
) -> Result<Vec<FollowingRow>, DbError> {
    let rows = sqlx::query_as!(
        FollowingRow,
        r#"
        SELECT f.id AS cursor, a.username,
               CASE WHEN a.portable THEN NULL ELSE a.domain END AS domain,
               f.show_reblogs, f.notify, f.languages, f.with_replies
        FROM follows f
        JOIN accounts a ON a.id = f.target_account_id
        WHERE f.account_id = $1 AND NOT f.pending
          AND ($2::bigint IS NULL OR f.id < $2)
        ORDER BY f.id DESC
        LIMIT $3
        "#,
        account_id,
        after,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Accounts `account_id` blocks, newest first.
pub async fn blocked_accounts(
    pool: &PgPool,
    account_id: i64,
    after: Option<i64>,
    limit: i64,
) -> Result<Vec<AcctRow>, DbError> {
    let rows = sqlx::query_as!(
        AcctRow,
        r#"
        SELECT b.id AS cursor, a.username,
               CASE WHEN a.portable THEN NULL ELSE a.domain END AS domain
        FROM blocks b
        JOIN accounts a ON a.id = b.target_account_id
        WHERE b.account_id = $1 AND ($2::bigint IS NULL OR b.id < $2)
        ORDER BY b.id DESC
        LIMIT $3
        "#,
        account_id,
        after,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Accounts `account_id` mutes (expired mutes excluded — they count as no mute
/// everywhere), newest first.
pub async fn muted_accounts(
    pool: &PgPool,
    account_id: i64,
    after: Option<i64>,
    limit: i64,
) -> Result<Vec<MutedRow>, DbError> {
    let rows = sqlx::query_as!(
        MutedRow,
        r#"
        SELECT m.id AS cursor, a.username,
               CASE WHEN a.portable THEN NULL ELSE a.domain END AS domain,
               m.hide_notifications
        FROM mutes m
        JOIN accounts a ON a.id = m.target_account_id
        WHERE m.account_id = $1
          AND (m.expires_at IS NULL OR m.expires_at > now())
          AND ($2::bigint IS NULL OR m.id < $2)
        ORDER BY m.id DESC
        LIMIT $3
        "#,
        account_id,
        after,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Statuses `account_id` has bookmarked, newest first. Rows whose status has
/// since vanished are dropped by the join (Mastodon skips absent statuses).
/// The CSV download passes [`EXPORT_ROW_LIMIT`]; the account archive and import
/// reconciliation pass [`UNLIMITED`].
pub async fn bookmarks(
    pool: &PgPool,
    account_id: i64,
    after: Option<i64>,
    limit: i64,
) -> Result<Vec<BookmarkRow>, DbError> {
    let rows = sqlx::query_as!(
        BookmarkRow,
        r#"
        SELECT bk.id AS cursor, s.uri, author.username AS author_username,
               author.uri AS author_uri, s.id AS status_id
        FROM bookmarks bk
        JOIN statuses s ON s.id = bk.status_id -- STUBKEEP: the export is the user's own record; a soft-deleted stub's URI still says what was bookmarked
        JOIN accounts author ON author.id = s.account_id
        WHERE bk.account_id = $1 AND ($2::bigint IS NULL OR bk.id < $2)
        ORDER BY bk.id DESC
        LIMIT $3
        "#,
        account_id,
        after,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One page of the statuses `account_id` has favourited, newest first — the
/// `likes.json` stream of the account archive. `after` is the `favourites.id`
/// cursor of the previous page's last row (`None` starts at the newest). Rows
/// whose status has since vanished are dropped by the join. The archive must be
/// complete, but paging it (rather than one unbounded read) keeps the builder's
/// memory bounded regardless of how many statuses the account has favourited.
pub async fn favourites_page(
    pool: &PgPool,
    account_id: i64,
    after: Option<i64>,
    limit: i64,
) -> Result<Vec<ExportRef>, DbError> {
    let rows = sqlx::query_as!(
        ExportRef,
        r#"
        SELECT fav.id AS cursor, s.uri, author.username AS author_username,
               author.uri AS author_uri, s.id AS status_id
        FROM favourites fav
        JOIN statuses s ON s.id = fav.status_id -- STUBKEEP: the export is the user's own record; a soft-deleted stub's URI still says what was favourited
        JOIN accounts author ON author.id = s.account_id
        WHERE fav.account_id = $1 AND ($2::bigint IS NULL OR fav.id < $2)
        ORDER BY fav.id DESC
        LIMIT $3
        "#,
        account_id,
        after,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One page of the statuses `account_id` has bookmarked, newest first — the
/// `bookmarks.json` stream of the account archive. `after` is the
/// `bookmarks.id` cursor of the previous page's last row (`None` starts at the
/// newest). The archive-only counterpart of [`bookmarks`], keyset-paged so the
/// builder never materialises the full bookmark set.
pub async fn bookmarks_page(
    pool: &PgPool,
    account_id: i64,
    after: Option<i64>,
    limit: i64,
) -> Result<Vec<ExportRef>, DbError> {
    let rows = sqlx::query_as!(
        ExportRef,
        r#"
        SELECT bk.id AS cursor, s.uri, author.username AS author_username,
               author.uri AS author_uri, s.id AS status_id
        FROM bookmarks bk
        JOIN statuses s ON s.id = bk.status_id -- STUBKEEP: the export is the user's own record; a soft-deleted stub's URI still says what was bookmarked
        JOIN accounts author ON author.id = s.account_id
        WHERE bk.account_id = $1 AND ($2::bigint IS NULL OR bk.id < $2)
        ORDER BY bk.id DESC
        LIMIT $3
        "#,
        account_id,
        after,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Domains `account_id` has blocked (user-level), newest first. The CSV
/// download passes [`EXPORT_ROW_LIMIT`]; import reconciliation passes
/// [`UNLIMITED`] because it must diff the full current set.
pub async fn blocked_domains(
    pool: &PgPool,
    account_id: i64,
    after: Option<i64>,
    limit: i64,
) -> Result<Vec<BlockedDomainRow>, DbError> {
    let rows = sqlx::query_as!(
        BlockedDomainRow,
        r#"
        SELECT id AS cursor, domain
        FROM account_domain_blocks
        WHERE account_id = $1 AND ($2::bigint IS NULL OR id < $2)
        ORDER BY id DESC
        LIMIT $3
        "#,
        account_id,
        after,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Every membership across `account_id`'s owned lists — one row per (list,
/// member) pair, grouped by list.
pub async fn lists(
    pool: &PgPool,
    account_id: i64,
    after: Option<(i64, i64)>,
    limit: i64,
) -> Result<Vec<ListMembershipRow>, DbError> {
    let (after_list, after_member) = after.unzip();
    let rows = sqlx::query_as!(
        ListMembershipRow,
        r#"
        SELECT l.id AS list_cursor, la.id AS member_cursor, l.title, a.username,
               CASE WHEN a.portable THEN NULL ELSE a.domain END AS domain
        FROM lists l
        JOIN list_accounts la ON la.list_id = l.id
        JOIN accounts a ON a.id = la.account_id
        WHERE l.account_id = $1
          AND ($2::bigint IS NULL OR (l.id, la.id) > ($2, $3))
        ORDER BY l.id, la.id
        LIMIT $4
        "#,
        account_id,
        after_list,
        after_member,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
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

    /// The export reads are keyset-paged: a page bounds the
    /// rows fetched, the last row's cursor resumes exactly where the page
    /// ended with no row lost or repeated, and the `None` + [`UNLIMITED`]
    /// combination the archive/import callers use still reads the whole set.
    #[sqlx::test]
    async fn export_queries_page_by_cursor_without_loss(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let total: i64 = 2_500;
        sqlx::query(
            "INSERT INTO account_domain_blocks (id, account_id, domain)
             SELECT g, $1, 'd' || g || '.example'
             FROM generate_series(1, $2) AS g",
        )
        .bind(alice)
        .bind(total)
        .execute(&pool)
        .await
        .unwrap();

        // Walk the whole set in pages of 1000 and collect every domain.
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let page = blocked_domains(&pool, alice, after, 1_000).await.unwrap();
            assert!(page.len() <= 1_000, "a page never exceeds its limit");
            let Some(last) = page.last() else { break };
            after = Some(last.cursor);
            let done = page.len() < 1_000;
            seen.extend(page.into_iter().map(|row| row.domain));
            if done {
                break;
            }
        }
        assert_eq!(seen.len(), usize::try_from(total).unwrap());
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            usize::try_from(total).unwrap(),
            "no row is repeated across page boundaries"
        );

        // The unpaged read the archive/import callers use sees the full set.
        let full = blocked_domains(&pool, alice, None, UNLIMITED)
            .await
            .unwrap();
        assert_eq!(full.len(), usize::try_from(total).unwrap());
    }
}
