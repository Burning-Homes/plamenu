//! CSV data export (Mastodon's `Settings::Exports`) for the first-party web UI.
//!
//! Six download endpoints reproduce Mastodon's export files byte-for-byte so an
//! exported archive can be re-imported anywhere that speaks the format: follows,
//! blocked/muted accounts, bookmarks, blocked domains, and lists. Each download
//! streams keyset-sized pages of rows into a chunked response body: no request
//! ever materialises the full dataset or its CSV rendering,
//! large accounts are never truncated, and the downloads draw from the
//! per-account web-maintenance admission budget before any query runs.

use std::future::Future;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_ap::urls::LocalStatusUrls;
use plamenu_db::archive::{self, AccountArchive};
use plamenu_db::export::{
    self, AcctRow, BlockedDomainRow, BookmarkRow, FollowingRow, ListMembershipRow, MutedRow,
};
use time::ext::NumericalDuration;

use super::clock::ViewerClock;
use super::i18n::Locale;
use super::session::{WebUser, csrf_rejection};
use super::settings::{bad_form, field, form_pairs, redirect_to};
use super::view::icon;
use crate::archive_worker::RETENTION_DAYS;
use crate::error::ApiError;
use crate::state::AppState;

/// The Import &amp; export page path (shared with [`super::import`]).
const EXPORT_PATH: &str = "/settings/export";
/// A new archive every 6 days — Mastodon's `BackupPolicy::MIN_AGE`.
pub(super) const ARCHIVE_MIN_AGE_DAYS: i32 = 6;
/// How many recent archives the settings page lists.
pub(super) const ARCHIVE_RECENT_LIMIT: i64 = 5;

/// One downloadable export: its Mastodon filename (also the last path segment)
/// and the catalog identifiers for the label/description shown on the page.
struct ExportFile {
    name: &'static str,
    label: &'static str,
    hint: &'static str,
}

const FILES: &[ExportFile] = &[
    ExportFile {
        name: "following_accounts.csv",
        label: "export-file-following",
        hint: "export-file-following-hint",
    },
    ExportFile {
        name: "lists.csv",
        label: "export-file-lists",
        hint: "export-file-lists-hint",
    },
    ExportFile {
        name: "blocked_accounts.csv",
        label: "export-file-blocked-accounts",
        hint: "export-file-blocked-accounts-hint",
    },
    ExportFile {
        name: "muted_accounts.csv",
        label: "export-file-muted-accounts",
        hint: "export-file-muted-accounts-hint",
    },
    ExportFile {
        name: "blocked_domains.csv",
        label: "export-file-blocked-domains",
        hint: "export-file-blocked-domains-hint",
    },
    ExportFile {
        name: "bookmarks.csv",
        label: "export-file-bookmarks",
        hint: "export-file-bookmarks-hint",
    },
];

/// The "Export" half of the Import &amp; export page — a list of CSV downloads.
/// Rendered by [`crate::web::import::page`], which owns the combined page.
pub(super) fn download_list(locale: Locale) -> Markup {
    html! {
        section.settings-form__group {
            h3.settings__subtitle { (locale.text("export-heading")) }
            p.settings-field__hint { (locale.text("export-intro")) }
            ul.export-list {
                @for file in FILES {
                    li.export-list__item {
                        div.export-list__meta {
                            span.export-list__label { (locale.text(file.label)) }
                            span.settings-field__hint { (locale.text(file.hint)) }
                        }
                        a.export-list__download download=(file.name)
                            href=(format!("/settings/export/{}", file.name)) {
                            (icon("download")) " " (locale.text("export-download-csv"))
                        }
                    }
                }
            }
        }
    }
}

/// The "Full archive" section of the Import &amp; export page: request a
/// complete account export and download finished ones. Mastodon's archive
/// half of `Settings::Exports`. `blocked` is the rate gate (a fresh request
/// within the last [`ARCHIVE_MIN_AGE_DAYS`] days).
pub(super) fn archive_section(
    archives: &[AccountArchive],
    blocked: bool,
    csrf: &str,
    clock: &ViewerClock,
    locale: Locale,
) -> Markup {
    let mut retention = FluentArgs::new();
    retention.set("days", RETENTION_DAYS);
    let mut cooldown = FluentArgs::new();
    cooldown.set("days", ARCHIVE_MIN_AGE_DAYS);
    html! {
        section.settings-form__group {
            h3.settings__subtitle { (locale.text("archive-heading")) }
            p.settings-field__hint { (locale.text_with("archive-intro", &retention)) }
            @if blocked {
                p.settings-field__hint {
                    (locale.text_with("archive-cooldown", &cooldown))
                    @if let Some(latest) = archives.first() {
                        " "
                        (next_available(latest, clock, locale))
                    }
                }
            } @else {
                form method="post" action="/web/settings/archive" {
                    input type="hidden" name="csrf" value=(csrf);
                    button type="submit" {
                        (icon("archive")) " " (locale.text("archive-request"))
                    }
                }
            }
            @if !archives.is_empty() {
                ul.export-list {
                    @for archive in archives {
                        li.export-list__item {
                            div.export-list__meta {
                                span.export-list__label {
                                    (locale.text("archive-label")) " · "
                                    (locale.text(archive_state_message(archive)))
                                }
                                span.settings-field__hint {
                                    (clock.element_date(archive.created_at))
                                    @if let Some(size) = archive.file_size {
                                        " · " (human_bytes(size, locale))
                                    }
                                }
                            }
                            @if archive.is_ready() {
                                a.export-list__download
                                    download=(format!("archive-{}.zip", archive.id))
                                    href=(format!("/settings/archive/{}/download", archive.id)) {
                                    (icon("download")) " " (locale.text("archive-download-zip"))
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The date the cooldown after `latest` lifts.
fn next_available(latest: &AccountArchive, clock: &ViewerClock, locale: Locale) -> String {
    let when = latest.created_at + i64::from(ARCHIVE_MIN_AGE_DAYS).days();
    let mut args = FluentArgs::new();
    args.set("date", clock.date(when));
    locale.text_with("archive-next", &args)
}

fn archive_state_message(archive: &AccountArchive) -> &'static str {
    match archive.state.as_str() {
        _ if archive.is_ready() => "archive-state-ready",
        "scheduled" => "archive-state-scheduled",
        "in_progress" => "archive-state-in-progress",
        // finished-but-file-missing: still terminal
        "finished" => "archive-state-ready",
        _ => "archive-state-unknown",
    }
}

/// A rough human byte size for the archive list (KB/MB, one decimal), using
/// integer math to keep the pedantic cast lints quiet. The unit and the way
/// the number is attached to it come from the catalog.
fn human_bytes(bytes: i64, locale: Locale) -> String {
    const KB: i64 = 1024;
    const MB: i64 = 1024 * 1024;
    let bytes = bytes.max(0);
    let (message, size) = if bytes < KB {
        ("size-bytes", bytes.to_string())
    } else if bytes < MB {
        (
            "size-kilobytes",
            format!("{}.{}", bytes / KB, (bytes % KB) * 10 / KB),
        )
    } else {
        (
            "size-megabytes",
            format!("{}.{}", bytes / MB, (bytes % MB) * 10 / MB),
        )
    };
    let mut args = FluentArgs::new();
    args.set("size", size);
    locale.text_with(message, &args)
}

/// `POST /web/settings/archive` — request a full account archive, subject to
/// the per-account rate limit ([`ARCHIVE_MIN_AGE_DAYS`]).
pub async fn request_archive(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let account_id = user.current.account.id;
    // Spend the per-account admission budget before any DB work: even
    // cooldown-refused spam must not repeatedly reach the advisory lock +
    // transaction below (finding #44). Legitimate use is far under the budget
    // (an archive is requestable once every `ARCHIVE_MIN_AGE_DAYS`).
    if let Err(error) = crate::rate_limit::check_web_maintenance(&state, account_id).await {
        return error.into_response();
    }
    // Atomically enforce the per-account cooldown and queue the archive: a
    // separate check-then-create races (finding #44), letting concurrent
    // requests schedule several full-archive builds for one account.
    match archive::create_if_none_within(&state.pool, account_id, ARCHIVE_MIN_AGE_DAYS).await {
        Ok(Some(_)) => redirect_to(&format!("{EXPORT_PATH}?saved=archive")),
        Ok(None) => redirect_to(&format!("{EXPORT_PATH}?error=archive_rate")),
        Err(error) => ApiError::from(error).into_response(),
    }
}

/// `GET /settings/archive/{id}/download` — stream a finished archive zip owned
/// by the signed-in user.
pub async fn download_archive(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let account_id = user.current.account.id;
    let Some(archive) = archive::find_for_account(&state.pool, account_id, id).await? else {
        return Err(ApiError::NotFound);
    };
    // Still building, or a finished row whose file has been swept.
    if !archive.is_ready() {
        return Err(ApiError::NotFound);
    }
    let Some(file_name) = archive.file_name else {
        return Err(ApiError::NotFound);
    };
    // Stream from the store with `Range` support rather than buffering the whole
    // ZIP into memory (finding #45): a large archive must not allocate a full
    // copy of itself for every concurrent download. `no-store` — an account
    // archive is private data and must not be held by any shared cache.
    let mut response = crate::routes::media::stream_stored_file(
        &state,
        &file_name,
        "application/zip",
        "no-store",
        &headers,
    )
    .await?;
    let disposition = format!("attachment; filename=\"archive-{id}.zip\"");
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).expect("numeric filename is a valid header"),
    );
    Ok(response)
}

/// Rows fetched (and rendered) per streamed chunk of a CSV download. One page
/// bounds both the query result and the CSV buffer in memory at any moment,
/// however many rows the account has.
const EXPORT_PAGE: i64 = 1_000;

/// The keyset cursor threaded between streamed pages. Single-key datasets use
/// the first element; `lists.csv` pages on a `(list id, membership id)` pair.
type PageCursor = (i64, i64);

/// The next page's cursor: `None` (end the stream) once a short page shows the
/// dataset is exhausted.
fn next_cursor(len: usize, last: Option<PageCursor>) -> Option<PageCursor> {
    (len >= usize::try_from(EXPORT_PAGE).unwrap_or(usize::MAX))
        .then_some(last)
        .flatten()
}

/// Streams keyset pages into a chunked `text/csv` response: `fetch` is called
/// with the previous page's cursor (`None` first) and returns the rendered
/// chunk plus the cursor to continue from, `None` ending the stream. A
/// database error after the first chunk cannot change the already-sent `200`;
/// it terminates the chunked body, which a client sees as a truncated
/// download.
fn stream_csv<F, Fut>(filename: &str, mut fetch: F) -> Response
where
    F: FnMut(Option<PageCursor>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(Vec<u8>, Option<PageCursor>), ApiError>> + Send + 'static,
{
    let stream = futures_util::stream::try_unfold(
        Some(None::<PageCursor>),
        move |state: Option<Option<PageCursor>>| {
            let fut = state.map(&mut fetch);
            async move {
                let Some(fut) = fut else { return Ok(None) };
                let (bytes, next) = fut.await?;
                Ok::<_, ApiError>(Some((Bytes::from(bytes), next.map(Some))))
            }
        },
    );
    let disposition = format!("attachment; filename=\"{filename}\"");
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/csv; charset=utf-8"),
            ),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&disposition).expect("fixed filename is a valid header"),
            ),
        ],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

/// `GET /settings/export/{file}` — stream one CSV download.
pub async fn download(
    State(state): State<AppState>,
    user: WebUser,
    Path(file): Path<String>,
) -> Result<Response, ApiError> {
    let account_id = user.current.account.id;
    // An export walks entire relationship datasets, so it draws from the
    // per-account web-maintenance admission budget before any query runs
    // (finding #47) — the same budget as the other expensive first-party
    // maintenance operations.
    crate::rate_limit::check_web_maintenance(&state, account_id).await?;
    let pool = state.pool.clone();
    let domain = state.config.domain.clone();
    let account_domain = state.config.account_domain.clone();

    // `file` is one of the six literals matched below, so it is a safe, fixed
    // filename — no header-injection risk in the disposition.
    let response = match file.as_str() {
        "following_accounts.csv" => stream_csv(&file, move |after| {
            let pool = pool.clone();
            let account_domain = account_domain.clone();
            async move {
                let rows = export::following(&pool, account_id, after.map(|(c, _)| c), EXPORT_PAGE)
                    .await?;
                let next = next_cursor(rows.len(), rows.last().map(|r| (r.cursor, 0)));
                Ok((following_csv(&rows, &account_domain, after.is_none()), next))
            }
        }),
        "blocked_accounts.csv" => stream_csv(&file, move |after| {
            let pool = pool.clone();
            let account_domain = account_domain.clone();
            async move {
                let rows =
                    export::blocked_accounts(&pool, account_id, after.map(|(c, _)| c), EXPORT_PAGE)
                        .await?;
                let next = next_cursor(rows.len(), rows.last().map(|r| (r.cursor, 0)));
                Ok((acct_only_csv(&rows, &account_domain), next))
            }
        }),
        "muted_accounts.csv" => stream_csv(&file, move |after| {
            let pool = pool.clone();
            let account_domain = account_domain.clone();
            async move {
                let rows =
                    export::muted_accounts(&pool, account_id, after.map(|(c, _)| c), EXPORT_PAGE)
                        .await?;
                let next = next_cursor(rows.len(), rows.last().map(|r| (r.cursor, 0)));
                Ok((muted_csv(&rows, &account_domain, after.is_none()), next))
            }
        }),
        "bookmarks.csv" => stream_csv(&file, move |after| {
            let pool = pool.clone();
            let domain = domain.clone();
            async move {
                let rows = export::bookmarks(&pool, account_id, after.map(|(c, _)| c), EXPORT_PAGE)
                    .await?;
                let next = next_cursor(rows.len(), rows.last().map(|r| (r.cursor, 0)));
                Ok((bookmarks_csv(&rows, &domain), next))
            }
        }),
        "blocked_domains.csv" => stream_csv(&file, move |after| {
            let pool = pool.clone();
            async move {
                let rows =
                    export::blocked_domains(&pool, account_id, after.map(|(c, _)| c), EXPORT_PAGE)
                        .await?;
                let next = next_cursor(rows.len(), rows.last().map(|r| (r.cursor, 0)));
                Ok((domains_csv(&rows), next))
            }
        }),
        "lists.csv" => stream_csv(&file, move |after| {
            let pool = pool.clone();
            let account_domain = account_domain.clone();
            async move {
                let rows = export::lists(&pool, account_id, after, EXPORT_PAGE).await?;
                let next = next_cursor(
                    rows.len(),
                    rows.last().map(|r| (r.list_cursor, r.member_cursor)),
                );
                Ok((lists_csv(&rows, &account_domain), next))
            }
        }),
        _ => return Err(ApiError::NotFound),
    };
    Ok(response)
}

/// A CSV writer matching Ruby's `CSV.generate`: `\n` row terminator and quoting
/// only when a field needs it (the `csv` crate's `Necessary` default). Shared
/// with the import layer's failures-CSV writer.
pub(super) fn writer() -> csv::Writer<Vec<u8>> {
    csv::WriterBuilder::new()
        .terminator(csv::Terminator::Any(b'\n'))
        .from_writer(Vec::new())
}

pub(super) fn finish(writer: csv::Writer<Vec<u8>>) -> Vec<u8> {
    writer
        .into_inner()
        .expect("in-memory CSV writer never fails to flush")
}

/// Renders an account address the way Mastodon's exporter does: local accounts
/// as `username@{local_domain}`, remote accounts as their stored `username@domain`.
fn acct(username: &str, domain: Option<&str>, local_domain: &str) -> String {
    format!("{username}@{}", domain.unwrap_or(local_domain))
}

/// Mastodon's four columns, in Mastodon's order, then ours. "Show replies" is
/// an extension: appending it keeps the file importable by Mastodon,
/// whose importer matches columns by header and ignores the ones it does not
/// know, and our own importer tolerates its absence.
fn following_csv(rows: &[FollowingRow], local_domain: &str, with_header: bool) -> Vec<u8> {
    let mut w = writer();
    if with_header {
        w.write_record([
            "Account address",
            "Show boosts",
            "Notify on new posts",
            "Languages",
            "Show replies",
        ])
        .expect("header write is infallible");
    }
    for row in rows {
        let address = acct(&row.username, row.domain.as_deref(), local_domain);
        // Languages join with ", " like Mastodon's exporter.
        let languages = row
            .languages
            .as_deref()
            .map(|langs| langs.join(", "))
            .unwrap_or_default();
        w.write_record([
            address.as_str(),
            if row.show_reblogs { "true" } else { "false" },
            if row.notify { "true" } else { "false" },
            &languages,
            if row.with_replies { "true" } else { "false" },
        ])
        .expect("row write is infallible");
    }
    finish(w)
}

fn acct_only_csv(rows: &[AcctRow], local_domain: &str) -> Vec<u8> {
    let mut w = writer();
    for row in rows {
        let address = acct(&row.username, row.domain.as_deref(), local_domain);
        w.write_record([address.as_str()])
            .expect("row write is infallible");
    }
    finish(w)
}

fn muted_csv(rows: &[MutedRow], local_domain: &str, with_header: bool) -> Vec<u8> {
    let mut w = writer();
    if with_header {
        w.write_record(["Account address", "Hide notifications"])
            .expect("header write is infallible");
    }
    for row in rows {
        let address = acct(&row.username, row.domain.as_deref(), local_domain);
        let hide = if row.hide_notifications {
            "true"
        } else {
            "false"
        };
        w.write_record([address.as_str(), hide])
            .expect("row write is infallible");
    }
    finish(w)
}

fn bookmarks_csv(rows: &[BookmarkRow], local_domain: &str) -> Vec<u8> {
    let mut w = writer();
    for row in rows {
        let uri = row.uri.clone().unwrap_or_else(|| {
            row.author_uri.as_ref().map_or_else(
                || LocalStatusUrls::new(local_domain, &row.author_username, row.status_id).id,
                |actor_id| {
                    LocalStatusUrls::from_actor_id(
                        local_domain,
                        &row.author_username,
                        actor_id,
                        row.status_id,
                    )
                    .id
                },
            )
        });
        w.write_record([uri.as_str()])
            .expect("row write is infallible");
    }
    finish(w)
}

fn domains_csv(rows: &[BlockedDomainRow]) -> Vec<u8> {
    let mut w = writer();
    for row in rows {
        w.write_record([row.domain.as_str()])
            .expect("row write is infallible");
    }
    finish(w)
}

fn lists_csv(rows: &[ListMembershipRow], local_domain: &str) -> Vec<u8> {
    let mut w = writer();
    for row in rows {
        let address = acct(&row.username, row.domain.as_deref(), local_domain);
        w.write_record([row.title.as_str(), address.as_str()])
            .expect("row write is infallible");
    }
    finish(w)
}

pub(super) fn csv_response(filename: &str, body: Vec<u8>) -> Response {
    let disposition = format!("attachment; filename=\"{filename}\"");
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/csv; charset=utf-8"),
            ),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&disposition).expect("fixed filename is a valid header"),
            ),
        ],
        body,
    )
        .into_response()
}
