//! CSV data-import processing — the parsing/coercion front end and the
//! worker-side service that applies a confirmed import.
//!
//! Parsing mirrors Mastodon's `Form::Import`: a file with a recognised first
//! header keeps its headers, otherwise it is reparsed with type-default headers
//! so a bare single-column list still imports; each cell is coerced (strip `@`,
//! lowercase domains, cast booleans) and sliced to the columns the chosen type
//! expects. Applying mirrors `BulkImportService` + `BulkImportRowService`: each
//! row runs through the normal follow/block/mute/domain-block/bookmark/list
//! actions (so federation happens exactly as it would interactively); a
//! successful row is deleted and the ones that remain are the failures.

use std::collections::HashSet;

use plamenu_ap::acct::Acct;
use plamenu_ap::urls::LocalStatusUrls;
use plamenu_db::account::{self, Account};
use plamenu_db::bulk_import::{self, BulkImport, BulkImportRow, ReconcileTarget};
use plamenu_db::{export, follow, list};
use serde_json::{Map, Value};

use crate::actions;
use crate::error::ApiError;
use crate::state::AppState;

/// The six CSV import kinds Plamenu supports (Mastodon's `custom_filters`
/// JSON import is out of scope).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportType {
    Following,
    Blocking,
    Muting,
    DomainBlocking,
    Bookmarks,
    Lists,
}

/// The recognised first-column headers across all types; their presence means
/// the file carries its own header row.
const KNOWN_FIRST_HEADERS: &[&str] = &["Account address", "#domain", "#uri", "List name"];

/// How many CSV rows a single import may carry (Mastodon's
/// `ROWS_PROCESSING_LIMIT`).
pub const ROWS_LIMIT: usize = 20_000;

/// The uploaded file's byte ceiling (Mastodon's `FILE_SIZE_LIMIT`).
pub const FILE_SIZE_LIMIT: usize = 20 * 1024 * 1024;

/// How many not-yet-finished imports (`unconfirmed` / `scheduled` /
/// `in_progress`) one account may hold at once (finding #51). Each import is
/// already capped at [`ROWS_LIMIT`] rows, so this bounds an account's
/// outstanding import rows at `MAX_UNFINISHED_IMPORTS_PER_ACCOUNT × ROWS_LIMIT`
/// and its pending worker/vacuum backlog with it — closing the vector where a
/// signed-in client repeatedly persists 20 MiB / 20,000-row files to accumulate
/// unbounded rows and work. It is generous for honest use (six import types
/// across merge/overwrite is ~12) while turning the previously unbounded growth
/// into a hard per-account ceiling. Mastodon imposes no such limit.
pub const MAX_UNFINISHED_IMPORTS_PER_ACCOUNT: i64 = 20;

/// How many rows a run applies before flushing the progress counters (Mastodon
/// bumps per row via separate jobs; we batch to one write per chunk).
const FLUSH_EVERY: i32 = 50;

impl ImportType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ImportType::Following => "following",
            ImportType::Blocking => "blocking",
            ImportType::Muting => "muting",
            ImportType::DomainBlocking => "domain_blocking",
            ImportType::Bookmarks => "bookmarks",
            ImportType::Lists => "lists",
        }
    }

    #[must_use]
    #[allow(clippy::should_implement_trait)] // fallible `Option`, not `FromStr`.
    pub fn from_str(value: &str) -> Option<Self> {
        Some(match value {
            "following" => ImportType::Following,
            "blocking" => ImportType::Blocking,
            "muting" => ImportType::Muting,
            "domain_blocking" => ImportType::DomainBlocking,
            "bookmarks" => ImportType::Bookmarks,
            "lists" => ImportType::Lists,
            _ => return None,
        })
    }

    /// The Mastodon filename for this type's failures download.
    #[must_use]
    pub fn failures_filename(self) -> &'static str {
        match self {
            ImportType::Following => "following_accounts_failures.csv",
            ImportType::Blocking => "blocked_accounts_failures.csv",
            ImportType::Muting => "muted_accounts_failures.csv",
            ImportType::DomainBlocking => "blocked_domains_failures.csv",
            ImportType::Bookmarks => "bookmarks_failures.csv",
            ImportType::Lists => "lists_failures.csv",
        }
    }

    /// The headers, in order, whose columns are kept for this type — the file's
    /// other columns are ignored (Mastodon's `EXPECTED_HEADERS_BY_TYPE`).
    fn expected_headers(self) -> &'static [&'static str] {
        match self {
            ImportType::Following => &[
                "Account address",
                "Show boosts",
                "Notify on new posts",
                "Languages",
                // Ours; Mastodon files simply lack it.
                "Show replies",
            ],
            ImportType::Blocking => &["Account address"],
            ImportType::Muting => &["Account address", "Hide notifications"],
            ImportType::DomainBlocking => &["#domain"],
            ImportType::Bookmarks => &["#uri"],
            ImportType::Lists => &["List name", "Account address"],
        }
    }

    /// The headers assumed for a headerless file, and the ones that must be
    /// present for a file to be compatible with this type (Mastodon's
    /// `default_csv_headers`).
    fn default_headers(self) -> &'static [&'static str] {
        match self {
            ImportType::Following | ImportType::Blocking | ImportType::Muting => {
                &["Account address"]
            }
            ImportType::DomainBlocking => &["#domain"],
            ImportType::Bookmarks => &["#uri"],
            ImportType::Lists => &["List name", "Account address"],
        }
    }
}

/// The JSON key a header column is coerced into (Mastodon's
/// `ATTRIBUTE_BY_HEADER`); `None` for columns no type keeps.
fn header_attr(header: &str) -> Option<&'static str> {
    Some(match header {
        "Account address" => "acct",
        "Show boosts" => "show_reblogs",
        // Ours, not Mastodon's: a following CSV without it imports
        // fine and leaves the actor-aware default in place.
        "Show replies" => "with_replies",
        "Notify on new posts" => "notify",
        "Languages" => "languages",
        "Hide notifications" => "hide_notifications",
        "#domain" => "domain",
        "#uri" => "uri",
        "List name" => "list_name",
        _ => return None,
    })
}

// ---- Parsing / coercion (unit-testable, no DB) -------------------------

/// Why an uploaded file was rejected outright (before any row runs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The file had no data rows.
    Empty,
    /// The file could not be read as CSV.
    Malformed,
    /// The file lacks the columns this type needs.
    IncompatibleType,
    /// The file exceeds [`ROWS_LIMIT`] rows.
    TooManyRows,
}

/// A parsed, coerced import ready to persist.
#[derive(Debug)]
pub struct ParsedCsv {
    /// One JSON object per data row, keyed by the coerced attribute names.
    pub rows: Vec<Value>,
    /// The effective header names (from the file, or the type defaults).
    pub headers: Vec<String>,
}

/// Casts a text field the way `ActiveModel::Type::Boolean` does: anything but
/// the recognised false spellings (and non-blank) is true.
fn cast_bool(raw: &str) -> bool {
    !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "0" | "f" | "false" | "off"
    )
}

/// Coerces one raw cell for a given header into its stored JSON value, matching
/// Mastodon's `csv_converter`. `None` drops the key (a blank optional field).
fn coerce(header: &str, raw: &str) -> Option<Value> {
    match header {
        "Account address" => {
            let cleaned = raw.trim().strip_prefix('@').unwrap_or(raw.trim());
            Some(Value::String(cleaned.to_owned()))
        }
        "Show boosts" | "Show replies" | "Notify on new posts" | "Hide notifications" => {
            (!raw.trim().is_empty()).then(|| Value::Bool(cast_bool(raw)))
        }
        "Languages" => {
            let langs: Vec<Value> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_owned()))
                .collect();
            (!langs.is_empty()).then_some(Value::Array(langs))
        }
        "#domain" => Some(Value::String(raw.trim().to_ascii_lowercase())),
        "#uri" | "List name" => Some(Value::String(raw.trim().to_owned())),
        _ => None,
    }
}

/// Parses and coerces an uploaded CSV for `import_type`, returning the rows to
/// store or a rejection reason.
pub fn parse_csv(import_type: ImportType, bytes: &[u8]) -> Result<ParsedCsv, ParseError> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .has_headers(false)
        .from_reader(bytes);

    let mut records: Vec<csv::StringRecord> = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|_| ParseError::Malformed)?;
        // Skip truly-blank lines (a lone empty field), matching Ruby CSV's
        // `skip_blanks`; a `,`-bearing all-empty row is kept.
        if record.len() <= 1 && record.get(0).is_none_or(str::is_empty) {
            continue;
        }
        records.push(record);
    }
    if records.is_empty() {
        return Err(ParseError::Empty);
    }

    // A recognised first cell means the file carries its own headers;
    // otherwise reparse under the type's default headers.
    let first_cell = records[0].get(0).unwrap_or_default().trim();
    let (headers, data): (Vec<String>, &[csv::StringRecord]) =
        if KNOWN_FIRST_HEADERS.contains(&first_cell) {
            let headers = records[0].iter().map(|h| h.trim().to_owned()).collect();
            (headers, &records[1..])
        } else {
            let headers = import_type
                .default_headers()
                .iter()
                .map(|h| (*h).to_owned())
                .collect();
            (headers, &records[..])
        };

    // The file must at least carry the columns this type needs.
    if !import_type
        .default_headers()
        .iter()
        .all(|needed| headers.iter().any(|h| h == needed))
    {
        return Err(ParseError::IncompatibleType);
    }
    if data.len() > ROWS_LIMIT {
        return Err(ParseError::TooManyRows);
    }

    let expected = import_type.expected_headers();
    let rows = data
        .iter()
        .map(|record| coerce_row(&headers, record, expected))
        .collect();
    Ok(ParsedCsv { rows, headers })
}

/// Coerces one data record into its stored JSON object, keeping only columns
/// whose header maps to an attribute this type expects.
fn coerce_row(headers: &[String], record: &csv::StringRecord, expected: &[&str]) -> Value {
    let mut object = Map::new();
    for (index, header) in headers.iter().enumerate() {
        let Some(attr) = header_attr(header) else {
            continue;
        };
        if !expected.contains(&header.as_str()) {
            continue;
        }
        if let Some(value) = coerce(header, record.get(index).unwrap_or_default()) {
            object.insert(attr.to_owned(), value);
        }
    }
    Value::Object(object)
}

/// Guesses the import type a file most resembles from its headers and filename
/// (Mastodon's `guessed_type`), so a likely wrong type choice can be flagged.
#[must_use]
pub fn guessed_type(headers: &[String], filename: &str) -> Option<ImportType> {
    let has = |name: &str| headers.iter().any(|h| h == name);
    let named = |prefix: &str| filename.starts_with(prefix);

    if has("Hide notifications") || named("mutes") || named("muted_accounts") {
        Some(ImportType::Muting)
    } else if has("Show boosts")
        || has("Notify on new posts")
        || has("Languages")
        || named("follows")
        || named("following_accounts")
    {
        Some(ImportType::Following)
    } else if named("blocks") || named("blocked_accounts") {
        Some(ImportType::Blocking)
    } else if named("domain_blocks") || named("blocked_domains") {
        Some(ImportType::DomainBlocking)
    } else if named("bookmarks") {
        Some(ImportType::Bookmarks)
    } else if named("lists") {
        Some(ImportType::Lists)
    } else {
        None
    }
}

/// Whether a parsed file looks like a different type than the one chosen.
#[must_use]
pub fn likely_mismatched(import_type: ImportType, headers: &[String], filename: &str) -> bool {
    guessed_type(headers, filename).is_some_and(|guess| guess != import_type)
}

// ---- Applying a confirmed import (worker side) -------------------------

/// Applies a claimed (`in_progress`) import: reconciles removals for overwrite
/// mode, then runs every row through the normal actions, deleting the rows
/// that succeed and leaving the failures. Marks the import `finished` at the
/// end. Row-level failures never abort the run — they simply stay as failures.
pub async fn run(state: &AppState, import: &BulkImport) -> Result<(), ApiError> {
    let Some(actor) = account::find_by_id(&state.pool, import.account_id).await? else {
        // The account vanished; nothing to import against.
        bulk_import::mark_finished(&state.pool, import.id).await?;
        return Ok(());
    };
    // A suspended account is being deleted or moderated: its
    // relationships are being purged and a `Delete(Actor)` has gone out, so
    // applying import rows would recreate follows/blocks/mutes and emit freshly
    // signed federation from an actor peers were told is gone. `claim_scheduled`
    // already skips suspended accounts; this catches the narrow window where the
    // account was suspended after this import was claimed. Cancel the import.
    if actor.suspended() {
        bulk_import::mark_finished(&state.pool, import.id).await?;
        return Ok(());
    }
    let Some(import_type) = ImportType::from_str(&import.import_type) else {
        bulk_import::mark_finished(&state.pool, import.id).await?;
        return Ok(());
    };

    let rows = bulk_import::rows(&state.pool, import.id).await?;
    if import.overwrite {
        reconcile(state, &actor, import_type, &rows).await?;
    }

    // Batch counter updates so the progress bar advances without a write per
    // row.
    let mut pending_processed = 0;
    let mut pending_imported = 0;
    for row in &rows {
        let imported = match Box::pin(process_row(state, &actor, import_type, row)).await {
            Ok(imported) => imported,
            Err(error) => {
                tracing::warn!(import = import.id, row = row.id, error = %error.chain(), "import row errored");
                false
            }
        };
        if imported {
            bulk_import::delete_row(&state.pool, row.id).await?;
        }
        pending_processed += 1;
        pending_imported += i32::from(imported);
        if pending_processed >= FLUSH_EVERY {
            bulk_import::bump_counts(&state.pool, import.id, pending_processed, pending_imported)
                .await?;
            pending_processed = 0;
            pending_imported = 0;
            // Deletion-race guard: a 20,000-row import can run for
            // a while, so re-check between flush windows whether the account was
            // suspended (self-deletion / moderation) mid-run and stop applying
            // rows if so — the remaining rows are cancelled, not left pending.
            if account::is_suspended(&state.pool, actor.id).await? {
                bulk_import::mark_finished(&state.pool, import.id).await?;
                return Ok(());
            }
        }
    }
    if pending_processed > 0 {
        bulk_import::bump_counts(&state.pool, import.id, pending_processed, pending_imported)
            .await?;
    }
    bulk_import::mark_finished(&state.pool, import.id).await?;
    Ok(())
}

/// Applies one row; `Ok(true)` means it imported (and its row can be deleted),
/// `Ok(false)` a soft failure the user can retry. Only infrastructure errors
/// bubble up as `Err`.
async fn process_row(
    state: &AppState,
    actor: &Account,
    import_type: ImportType,
    row: &BulkImportRow,
) -> Result<bool, ApiError> {
    match import_type {
        ImportType::Following => {
            let Some(target) = resolve_import_account(state, row.acct.as_deref()).await? else {
                return Ok(false);
            };
            if actions::follow_account(state, actor, &target)
                .await
                .is_err()
            {
                return Ok(false);
            }
            // Absent columns take Mastodon's per-follow defaults, so a
            // re-imported row always leaves the edge in the row's state.
            // `with_replies` is ours and has no Mastodon default to fall back
            // on: absent, it stays `None` so the follow keeps whatever the
            // `follows` trigger chose for this target.
            let show_reblogs = row.show_reblogs.unwrap_or(true);
            let notify = row.notify.unwrap_or(false);
            follow::update_settings(
                &state.pool,
                actor.id,
                target.id,
                Some(show_reblogs),
                row.with_replies,
                Some(notify),
                Some(&row.languages),
            )
            .await?;
            Ok(true)
        }
        ImportType::Blocking => {
            let Some(target) = resolve_import_account(state, row.acct.as_deref()).await? else {
                return Ok(false);
            };
            Ok(actions::block_account(state, actor, &target).await.is_ok())
        }
        ImportType::Muting => {
            let Some(target) = resolve_import_account(state, row.acct.as_deref()).await? else {
                return Ok(false);
            };
            // Absent column ⇒ Mastodon's `MuteService` default (hide).
            let hide = row.hide_notifications.unwrap_or(true);
            Ok(actions::mute_account(state, actor, &target, hide, 0)
                .await
                .is_ok())
        }
        ImportType::DomainBlocking => {
            let Some(domain) = row.domain.as_deref() else {
                return Ok(false);
            };
            if domain.is_empty() || state.config.is_local_domain(domain) {
                return Ok(false);
            }
            Ok(actions::block_domain(state, actor, domain).await.is_ok())
        }
        ImportType::Bookmarks => {
            let Some(uri) = row.uri.as_deref() else {
                return Ok(false);
            };
            let Some(status) = crate::ingest::resolve_or_fetch_status(state, uri).await? else {
                return Ok(false);
            };
            Ok(actions::bookmark_status(state, actor, status.id)
                .await
                .is_ok())
        }
        ImportType::Lists => process_list_row(state, actor, row).await,
    }
}

/// A list row: find-or-create the named list, ensure the follow, and add the
/// account (Mastodon's `list.accounts << target`, which needs the follow).
async fn process_list_row(
    state: &AppState,
    actor: &Account,
    row: &BulkImportRow,
) -> Result<bool, ApiError> {
    let Some(title) = row.list_name.as_deref() else {
        return Ok(false);
    };
    if title.is_empty() {
        return Ok(false);
    }
    let Some(target) = resolve_import_account(state, row.acct.as_deref()).await? else {
        return Ok(false);
    };
    let list = match list::find_owned_by_title(&state.pool, actor.id, title).await? {
        Some(list) => list,
        None => list::create(&state.pool, actor.id, title, "list", false).await?,
    };
    if target.id != actor.id {
        // Best effort: a failed follow still lets an existing edge add.
        let _ = actions::follow_account(state, actor, &target).await;
    }
    match list::add_members(&state.pool, list.id, actor.id, &[target.id]).await? {
        Ok(()) | Err(list::AddMemberError::AlreadyMember) => Ok(true),
        Err(list::AddMemberError::NotFollowed) => Ok(false),
    }
}

/// Resolves a row's `acct` field to a stored account: a local username, a known
/// remote account, or a fresh webfinger + actor fetch. `None` when it cannot be
/// resolved (a soft failure).
async fn resolve_import_account(
    state: &AppState,
    raw: Option<&str>,
) -> Result<Option<Account>, ApiError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let acct = raw.trim().trim_start_matches('@');
    if let Some((username, domain)) = acct.split_once('@') {
        if state.config.is_local_domain(domain) {
            let account = account::find_local_account_by_username(&state.pool, username).await?;
            return publicly_visible_account(state, account).await;
        }
        let Ok(parsed) = acct.parse::<Acct>() else {
            return Ok(None);
        };
        // Imported follows/mutes/blocks name person-like actors.
        return Ok(crate::remote::resolve_remote_account(
            state,
            &parsed,
            account::ActorClass::PersonLike,
        )
        .await?);
    }
    // A bare username is one of ours.
    let account = account::find_local_account_by_username(&state.pool, acct).await?;
    publicly_visible_account(state, account).await
}

async fn publicly_visible_account(
    state: &AppState,
    account: Option<Account>,
) -> Result<Option<Account>, ApiError> {
    let Some(account) = account else {
        return Ok(None);
    };
    if !crate::instance_policy::public_account_visible(&state.pool, &state.config.domain, &account)
        .await?
    {
        return Ok(None);
    }
    Ok(Some(account))
}

// ---- Overwrite reconciliation ------------------------------------------

/// Removes relationships absent from the uploaded file (overwrite mode), via
/// the normal actions so `Undo` activities federate. Counts are untouched:
/// every file row is still applied afterwards and counted there.
async fn reconcile(
    state: &AppState,
    actor: &Account,
    import_type: ImportType,
    rows: &[BulkImportRow],
) -> Result<(), ApiError> {
    let domain = &state.config.account_domain;
    match import_type {
        ImportType::Following => {
            let keep = acct_keys(rows, domain);
            for target in bulk_import::following_targets(&state.pool, actor.id).await? {
                if !keep.contains(&target_key(&target, domain)) {
                    remove_relationship(state, actor, &target, Relationship::Follow).await?;
                }
            }
        }
        ImportType::Blocking => {
            let keep = acct_keys(rows, domain);
            for target in bulk_import::blocking_targets(&state.pool, actor.id).await? {
                if !keep.contains(&target_key(&target, domain)) {
                    remove_relationship(state, actor, &target, Relationship::Block).await?;
                }
            }
        }
        ImportType::Muting => {
            let keep = acct_keys(rows, domain);
            for target in bulk_import::muting_targets(&state.pool, actor.id).await? {
                if !keep.contains(&target_key(&target, domain)) {
                    remove_relationship(state, actor, &target, Relationship::Mute).await?;
                }
            }
        }
        ImportType::DomainBlocking => {
            let keep: HashSet<String> = rows
                .iter()
                .filter_map(|row| row.domain.as_deref())
                .map(str::to_ascii_lowercase)
                .collect();
            // Overwrite reconciliation diffs the full current set, so read all
            // of it rather than a bounded export page.
            for blocked in
                export::blocked_domains(&state.pool, actor.id, None, export::UNLIMITED).await?
            {
                if !keep.contains(&blocked.domain) {
                    actions::unblock_domain(state, actor, &blocked.domain).await?;
                }
            }
        }
        ImportType::Bookmarks => {
            let keep: HashSet<&str> = rows.iter().filter_map(|row| row.uri.as_deref()).collect();
            for bookmark in
                export::bookmarks(&state.pool, actor.id, None, export::UNLIMITED).await?
            {
                let uri = bookmark.uri.clone().unwrap_or_else(|| {
                    bookmark.author_uri.as_ref().map_or_else(
                        || {
                            LocalStatusUrls::new(
                                domain,
                                &bookmark.author_username,
                                bookmark.status_id,
                            )
                            .id
                        },
                        |actor_id| {
                            LocalStatusUrls::from_actor_id(
                                domain,
                                &bookmark.author_username,
                                actor_id,
                                bookmark.status_id,
                            )
                            .id
                        },
                    )
                });
                if !keep.contains(uri.as_str()) {
                    actions::unbookmark_status(state, actor, bookmark.status_id).await?;
                }
            }
        }
        ImportType::Lists => {
            let titles: Vec<String> = rows
                .iter()
                .filter_map(|row| row.list_name.as_deref())
                .map(str::to_owned)
                .collect();
            list::delete_owned_not_in_titles(&state.pool, actor.id, &titles).await?;
            list::clear_owned_memberships(&state.pool, actor.id).await?;
        }
    }
    Ok(())
}

/// Which relationship a reconcile removal severs.
#[derive(Clone, Copy)]
enum Relationship {
    Follow,
    Block,
    Mute,
}

/// Fetches the target account and undoes the given relationship.
async fn remove_relationship(
    state: &AppState,
    actor: &Account,
    target: &ReconcileTarget,
    kind: Relationship,
) -> Result<(), ApiError> {
    let Some(account) = account::find_by_id(&state.pool, target.account_id).await? else {
        return Ok(());
    };
    match kind {
        Relationship::Follow => actions::unfollow_account(state, actor, &account).await,
        Relationship::Block => actions::unblock_account(state, actor, &account).await,
        Relationship::Mute => actions::unmute_account(state, actor, &account).await,
    }
}

/// The set of comparison keys the uploaded account rows want kept.
fn acct_keys(rows: &[BulkImportRow], local_domain: &str) -> HashSet<String> {
    rows.iter()
        .filter_map(|row| row.acct.as_deref())
        .map(|acct| acct_key_from_str(acct, local_domain))
        .collect()
}

/// A reconcile key for a stored relationship target.
fn target_key(target: &ReconcileTarget, local_domain: &str) -> String {
    acct_key(&target.username, target.domain.as_deref(), local_domain)
}

/// Canonicalises an account into a comparison key: local accounts collapse to a
/// bare lowercase username, remote ones to `username@domain` (both lowercased),
/// so the same account matches whether the file used the local suffix or not.
fn acct_key(username: &str, domain: Option<&str>, local_domain: &str) -> String {
    match domain.filter(|d| !d.eq_ignore_ascii_case(local_domain)) {
        Some(remote) => format!(
            "{}@{}",
            username.to_ascii_lowercase(),
            remote.to_ascii_lowercase()
        ),
        None => username.to_ascii_lowercase(),
    }
}

/// The reconcile key for a file's `acct` string (`@` stripped, split on the
/// first `@`).
fn acct_key_from_str(acct: &str, local_domain: &str) -> String {
    let acct = acct.trim().trim_start_matches('@');
    match acct.split_once('@') {
        Some((username, domain)) => acct_key(username, Some(domain), local_domain),
        None => acct_key(acct, None, local_domain),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(parsed: &ParsedCsv) -> Vec<&str> {
        parsed.headers.iter().map(String::as_str).collect()
    }

    #[test]
    fn parses_mastodon_following_headers() {
        let csv = "Account address,Show boosts,Notify on new posts,Languages\n\
                   @bob@example.com,true,false,\n\
                   carol@remote.test,false,true,\"en, fr\"\n";
        let parsed = parse_csv(ImportType::Following, csv.as_bytes()).unwrap();
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.rows[0]["acct"], "bob@example.com");
        assert_eq!(parsed.rows[0]["show_reblogs"], true);
        assert_eq!(parsed.rows[0]["notify"], false);
        // Blank languages column drops the key.
        assert!(parsed.rows[0].get("languages").is_none());
        assert_eq!(parsed.rows[1]["show_reblogs"], false);
        assert_eq!(parsed.rows[1]["languages"], serde_json::json!(["en", "fr"]));
    }

    #[test]
    fn headerless_single_column_uses_defaults() {
        // A bare blocked-accounts file (no header row) still parses.
        let csv = "@spammer@bad.test\neve@evil.test\n";
        let parsed = parse_csv(ImportType::Blocking, csv.as_bytes()).unwrap();
        assert_eq!(headers(&parsed), vec!["Account address"]);
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.rows[0]["acct"], "spammer@bad.test");
        assert_eq!(parsed.rows[1]["acct"], "eve@evil.test");
    }

    #[test]
    fn mutes_keep_hide_notifications() {
        let csv = "Account address,Hide notifications\nbob@example.com,true\ncarol@example.com,\n";
        let parsed = parse_csv(ImportType::Muting, csv.as_bytes()).unwrap();
        assert_eq!(parsed.rows[0]["hide_notifications"], true);
        // Blank ⇒ absent key ⇒ the service defaults it to hide.
        assert!(parsed.rows[1].get("hide_notifications").is_none());
    }

    #[test]
    fn lists_headerless_two_columns() {
        let csv = "Friends,bob@example.com\n\"Work, folks\",carol@remote.test\n";
        let parsed = parse_csv(ImportType::Lists, csv.as_bytes()).unwrap();
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.rows[0]["list_name"], "Friends");
        assert_eq!(parsed.rows[0]["acct"], "bob@example.com");
        assert_eq!(parsed.rows[1]["list_name"], "Work, folks");
    }

    #[test]
    fn domain_lowercased_and_stripped() {
        let csv = "#domain\nBad.Example\n  spam.test  \n";
        let parsed = parse_csv(ImportType::DomainBlocking, csv.as_bytes()).unwrap();
        assert_eq!(parsed.rows[0]["domain"], "bad.example");
        assert_eq!(parsed.rows[1]["domain"], "spam.test");
    }

    #[test]
    fn incompatible_type_is_rejected() {
        // A domain file cannot import as a bookmarks list.
        let csv = "#domain\nspam.test\n";
        assert_eq!(
            parse_csv(ImportType::Bookmarks, csv.as_bytes()).unwrap_err(),
            ParseError::IncompatibleType
        );
    }

    #[test]
    fn empty_file_is_rejected() {
        assert_eq!(
            parse_csv(ImportType::Following, b"\n\n").unwrap_err(),
            ParseError::Empty
        );
    }

    #[test]
    fn guessed_type_and_mismatch() {
        // A mutes file imported as following is flagged.
        let headers = vec![
            "Account address".to_owned(),
            "Hide notifications".to_owned(),
        ];
        assert_eq!(
            guessed_type(&headers, "muted_accounts.csv"),
            Some(ImportType::Muting)
        );
        assert!(likely_mismatched(
            ImportType::Following,
            &headers,
            "muted_accounts.csv"
        ));
        assert!(!likely_mismatched(
            ImportType::Muting,
            &headers,
            "muted_accounts.csv"
        ));
        // A plain single-column blocks file, chosen as blocking, is fine.
        let plain = vec!["Account address".to_owned()];
        assert!(!likely_mismatched(
            ImportType::Blocking,
            &plain,
            "blocked_accounts.csv"
        ));
    }

    #[test]
    fn acct_key_collapses_local_suffix() {
        assert_eq!(
            acct_key_from_str("@Bob@plamenu.test", "plamenu.test"),
            "bob"
        );
        assert_eq!(acct_key_from_str("bob", "plamenu.test"), "bob");
        assert_eq!(
            acct_key_from_str("carol@Remote.Test", "plamenu.test"),
            "carol@remote.test"
        );
    }
}
