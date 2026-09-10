//! CSV data import for the first-party web UI — the "Import" half of
//! the Import &amp; export settings page.
//!
//! An upload is parsed and coerced ([`crate::bulk_import::parse_csv`]) into an
//! `unconfirmed` import the user then reviews and confirms; the import worker
//! applies it. The page also lists recent imports with their progress and a
//! failures download. The confirm/apply split matches Mastodon: parsing is
//! cheap and synchronous, applying (which federates follows/blocks/…) runs in
//! the background.

use axum::body::Bytes;
use axum::extract::{Multipart, Path, Query, State};
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::bulk_import::{
    self as store, BulkImport, BulkImportRow, ImportAdmission, MAX_GLOBAL_PENDING_IMPORT_ROWS,
};

use super::export;
use super::i18n::Locale;
use super::session::{WebUser, csrf_rejection};
use super::settings::{
    SettingsQuery, bad_form, error_flash, field, form_pairs, redirect_to, saved_flash,
    settings_shell,
};
use super::view::icon;
use crate::bulk_import::{
    FILE_SIZE_LIMIT, ImportType, MAX_UNFINISHED_IMPORTS_PER_ACCOUNT, ParseError, likely_mismatched,
    parse_csv,
};
use crate::error::ApiError;
use crate::state::AppState;

/// The path the Import &amp; export page lives at (shared with export).
const PATH: &str = "/settings/export";
/// How many recent imports the page lists (Mastodon's `RECENT_IMPORTS_LIMIT`).
const RECENT_LIMIT: i64 = 10;

/// The import types offered in the upload form, in Mastodon's order.
const TYPES: &[ImportType] = &[
    ImportType::Following,
    ImportType::Blocking,
    ImportType::Muting,
    ImportType::DomainBlocking,
    ImportType::Bookmarks,
    ImportType::Lists,
];

/// `GET /settings/export` — the combined Import &amp; export page.
pub async fn page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<SettingsQuery>,
) -> Result<Markup, ApiError> {
    let account_id = user.current.account.id;
    let recent = store::recent_for_account(&state.pool, account_id, RECENT_LIMIT).await?;
    let archives = plamenu_db::archive::recent_for_account(
        &state.pool,
        account_id,
        export::ARCHIVE_RECENT_LIMIT,
    )
    .await?;
    let archive_blocked =
        plamenu_db::archive::created_within(&state.pool, account_id, export::ARCHIVE_MIN_AGE_DAYS)
            .await?;
    let clock = &user.clock;
    let locale = user.locale;
    let body = html! {
        (saved_flash(
            query.saved.is_some(),
            &locale.text(saved_message(query.saved.as_deref())),
        ))
        (error_flash(
            query.error.as_deref().and_then(error_message)
                .map(|id| locale.text(id)).as_deref(),
        ))
        // The grid wrapper spaces the section cards — bare siblings would sit
        // border against border.
        div.settings-form {
            (import_form(&user, locale))
            (recent_imports(&recent, locale))
            (export::download_list(locale))
            (export::archive_section(&archives, archive_blocked, &user.csrf, clock, locale))
        }
    };
    Ok(settings_shell(
        &user,
        PATH,
        &locale.text("export-page-title"),
        &body,
    ))
}

/// The upload form: pick a type and mode, choose a CSV.
fn import_form(user: &WebUser, locale: Locale) -> Markup {
    html! {
        form.settings-form__group method="post" action="/web/settings/import"
            enctype="multipart/form-data" {
            h3.settings__subtitle { (locale.text("import-heading")) }
            input type="hidden" name="csrf" value=(user.csrf);
            label.settings-field {
                span.settings-field__label { (locale.text("import-type-label")) }
                select name="type" {
                    @for import_type in TYPES {
                        option value=(import_type.as_str()) {
                            (locale.text(type_message(*import_type)))
                        }
                    }
                }
            }
            label.settings-field {
                span.settings-field__label { (locale.text("import-file-label")) }
                input type="file" name="data" accept=".csv,text/csv" required;
                span.settings-field__hint { (locale.text("import-file-hint")) }
            }
            fieldset.settings-form__group {
                legend { (locale.text("import-mode-legend")) }
                label.settings-field__choice {
                    input type="radio" name="mode" value="merge" checked;
                    span { (locale.text("import-mode-merge")) }
                }
                label.settings-field__choice {
                    input type="radio" name="mode" value="overwrite";
                    span { (locale.text("import-mode-overwrite")) }
                }
            }
            button type="submit" { (icon("upload")) " " (locale.text("import-upload")) }
        }
    }
}

/// The recent-imports list with per-import status and a failures download.
fn recent_imports(imports: &[BulkImport], locale: Locale) -> Markup {
    html! {
        @if !imports.is_empty() {
            section.settings-form__group {
                h3.settings__subtitle { (locale.text("import-recent-heading")) }
                ul.export-list {
                    @for import in imports {
                        li.export-list__item {
                            div.export-list__meta {
                                span.export-list__label {
                                    (locale.text(type_message_of(&import.import_type)))
                                    " · "
                                    (locale.text(state_message(import)))
                                }
                                span.settings-field__hint { (import_detail(import, locale)) }
                            }
                            (import_actions(import, locale))
                        }
                    }
                }
            }
        }
    }
}

/// The links/forms for one import: review (unconfirmed) or failures download.
fn import_actions(import: &BulkImport, locale: Locale) -> Markup {
    html! {
        div.export-list__actions {
            @if import.state == "unconfirmed" {
                a.export-list__download href=(format!("/settings/import/{}", import.id)) {
                    (locale.text("import-review"))
                }
            }
            @if import.state == "finished" && import.failure_count() > 0 {
                a.export-list__download
                    href=(format!("/settings/import/{}/failures.csv", import.id)) {
                    (icon("download")) " " (locale.text("import-failures"))
                }
            }
        }
    }
}

/// `POST /web/settings/import` — parse an uploaded CSV into an unconfirmed
/// import and send the user to its review page.
pub async fn upload(
    State(state): State<AppState>,
    user: WebUser,
    crate::instance_policy::RemoteIp(ip): crate::instance_policy::RemoteIp,
    mut multipart: Multipart,
) -> Response {
    let account_id = user.current.account.id;
    // Admission budget first, before the ~21 MiB body is buffered: a per-account
    // and per-IP request ceiling turns an upload flood away up front, so a burst
    // of simultaneous uploads can't each read a full body into memory before the
    // DB gate refuses them (finding #51).
    if let Err(error) = crate::rate_limit::check_import_upload(&state, account_id, ip).await {
        return error.into_response();
    }
    // Reject an over-quota account before buffering and parsing the ~21 MiB body
    // (finding #51). This is a cheap best-effort pre-check; `create_with_rows_capped`
    // below is the authoritative, race-free gate that also catches a burst of
    // simultaneous first uploads this count cannot see.
    match store::count_unfinished_for_account(&state.pool, account_id).await {
        Ok(count) if count >= MAX_UNFINISHED_IMPORTS_PER_ACCOUNT => {
            return redirect_to(&format!("{PATH}?error=too_many_pending"));
        }
        Ok(_) => {}
        Err(error) => return ApiError::from(error).into_response(),
    }

    let mut csrf = String::new();
    let mut type_str = String::new();
    let mut mode = String::new();
    let mut filename = String::new();
    let mut data: Vec<u8> = Vec::new();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => return bad_form(format!("invalid form: {error}")),
        };
        match field.name().unwrap_or_default() {
            "csrf" => csrf = field.text().await.unwrap_or_default(),
            "type" => type_str = field.text().await.unwrap_or_default(),
            "mode" => mode = field.text().await.unwrap_or_default(),
            "data" => {
                filename = field.file_name().unwrap_or_default().to_owned();
                match field.bytes().await {
                    Ok(bytes) => data = bytes.to_vec(),
                    Err(error) => return bad_form(format!("upload failed: {error}")),
                }
            }
            _ => {
                let _ = field.text().await;
            }
        }
    }

    if !user.csrf_ok(&csrf) {
        return csrf_rejection();
    }
    let Some(import_type) = ImportType::from_str(&type_str) else {
        return redirect_to(&format!("{PATH}?error=bad_type"));
    };
    if data.len() > FILE_SIZE_LIMIT {
        return redirect_to(&format!("{PATH}?error=too_large"));
    }
    let parsed = match parse_csv(import_type, &data) {
        Ok(parsed) => parsed,
        Err(error) => return redirect_to(&format!("{PATH}?error={}", error_code(&error))),
    };
    let mismatched = likely_mismatched(import_type, &parsed.headers, &filename);
    match store::create_with_rows_capped(
        &state.pool,
        account_id,
        import_type.as_str(),
        mode == "overwrite",
        &filename,
        mismatched,
        &parsed.rows,
        MAX_UNFINISHED_IMPORTS_PER_ACCOUNT,
        MAX_GLOBAL_PENDING_IMPORT_ROWS,
    )
    .await
    {
        Ok(ImportAdmission::Admitted(import)) => {
            redirect_to(&format!("/settings/import/{}", import.id))
        }
        Ok(ImportAdmission::AccountFull) => redirect_to(&format!("{PATH}?error=too_many_pending")),
        Ok(ImportAdmission::ServerBusy) => redirect_to(&format!("{PATH}?error=server_busy")),
        Err(error) => ApiError::from(error).into_response(),
    }
}

/// `GET /settings/import/{id}` — review and confirm an unconfirmed import.
pub async fn show(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let Some(import) = store::find_for_account(&state.pool, user.current.account.id, id).await?
    else {
        return Err(ApiError::NotFound);
    };
    // Already confirmed/processing/finished: nothing to review.
    if import.state != "unconfirmed" {
        return Ok(redirect_to(PATH));
    }
    let locale = user.locale;
    let mode = if import.overwrite {
        "import-summary-mode-overwrite"
    } else {
        "import-summary-mode-merge"
    };
    let body = html! {
        p.settings-field__hint { (locale.text("import-review-hint")) }
        dl.settings-summary {
            dt { (locale.text("import-summary-file")) } dd { (import.original_filename) }
            dt { (locale.text("import-summary-type")) }
            dd { (locale.text(type_message_of(&import.import_type))) }
            dt { (locale.text("import-summary-mode")) } dd { (locale.text(mode)) }
            dt { (locale.text("import-summary-rows")) } dd { (import.total_items) }
        }
        @if import.likely_mismatched {
            p.settings__error role="alert" {
                (icon("alert")) " " (locale.text("import-mismatch-warning"))
            }
        }
        div.settings-form__actions {
            form method="post" action=(format!("/web/settings/import/{}/confirm", import.id)) {
                input type="hidden" name="csrf" value=(user.csrf);
                button type="submit" { (locale.text("import-confirm")) }
            }
            form method="post" action=(format!("/web/settings/import/{}/delete", import.id)) {
                input type="hidden" name="csrf" value=(user.csrf);
                button.settings-button--danger type="submit" { (locale.text("import-discard")) }
            }
        }
    };
    Ok(settings_shell(&user, PATH, &locale.text("import-review-title"), &body).into_response())
}

/// `POST /web/settings/import/{id}/confirm` — schedule an unconfirmed import.
pub async fn confirm_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    match store::mark_scheduled(&state.pool, user.current.account.id, id).await {
        Ok(true) => redirect_to(&format!("{PATH}?saved=import")),
        Ok(false) => redirect_to(PATH),
        Err(error) => ApiError::from(error).into_response(),
    }
}

/// `POST /web/settings/import/{id}/delete` — discard an import.
pub async fn delete_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    match store::delete_for_account(&state.pool, user.current.account.id, id).await {
        Ok(_) => redirect_to(PATH),
        Err(error) => ApiError::from(error).into_response(),
    }
}

/// `GET /settings/import/{id}/failures.csv` — the rows that failed to import,
/// re-emitted in the same shape as the corresponding export.
pub async fn failures(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let Some(import) = store::find_for_account(&state.pool, user.current.account.id, id).await?
    else {
        return Err(ApiError::NotFound);
    };
    // Failures only exist once processing has finished.
    if import.state != "finished" {
        return Err(ApiError::NotFound);
    }
    let import_type = ImportType::from_str(&import.import_type).ok_or(ApiError::NotFound)?;
    let rows = store::rows(&state.pool, import.id).await?;
    Ok(export::csv_response(
        import_type.failures_filename(),
        failures_csv(import_type, &rows),
    ))
}

/// Builds the `{type}_failures.csv` body from the leftover rows, matching
/// Mastodon's `Settings::ImportsController#failures` shape (headers only for
/// following and muting).
fn failures_csv(import_type: ImportType, rows: &[BulkImportRow]) -> Vec<u8> {
    let mut w = export::writer();
    match import_type {
        ImportType::Following => {
            w.write_record([
                "Account address",
                "Show boosts",
                "Notify on new posts",
                "Languages",
                "Show replies",
            ])
            .expect("header write is infallible");
            for row in rows {
                let languages = row.languages.join(", ");
                // A blank replies cell round-trips as "no opinion" — the
                // re-import leaves the follows trigger's actor-aware default
                // alone, exactly as the original upload did.
                w.write_record([
                    row.acct.as_deref().unwrap_or(""),
                    bool_field(row.show_reblogs, true),
                    bool_field(row.notify, false),
                    &languages,
                    match row.with_replies {
                        Some(true) => "true",
                        Some(false) => "false",
                        None => "",
                    },
                ])
                .expect("row write is infallible");
            }
        }
        ImportType::Muting => {
            w.write_record(["Account address", "Hide notifications"])
                .expect("header write is infallible");
            for row in rows {
                w.write_record([
                    row.acct.as_deref().unwrap_or(""),
                    bool_field(row.hide_notifications, true),
                ])
                .expect("row write is infallible");
            }
        }
        ImportType::Blocking => write_single(&mut w, rows, "acct"),
        ImportType::DomainBlocking => write_single(&mut w, rows, "domain"),
        ImportType::Bookmarks => write_single(&mut w, rows, "uri"),
        ImportType::Lists => {
            for row in rows {
                w.write_record([
                    row.list_name.as_deref().unwrap_or(""),
                    row.acct.as_deref().unwrap_or(""),
                ])
                .expect("row write is infallible");
            }
        }
    }
    export::finish(w)
}

/// Writes a headerless single-column CSV of one field per row.
fn write_single(w: &mut csv::Writer<Vec<u8>>, rows: &[BulkImportRow], key: &str) {
    for row in rows {
        let value = match key {
            "acct" => row.acct.as_deref(),
            "domain" => row.domain.as_deref(),
            "uri" => row.uri.as_deref(),
            _ => None,
        }
        .unwrap_or("");
        w.write_record([value]).expect("row write is infallible");
    }
}

/// A boolean field re-serialised as Ruby's CSV would, defaulting an absent key.
fn bool_field(value: Option<bool>, default: bool) -> &'static str {
    if value.unwrap_or(default) {
        "true"
    } else {
        "false"
    }
}

// ---- Presentation helpers ----------------------------------------------

/// The catalog identifier naming an import type in the picker and the recent
/// list. This is the only place the six types are named for a reader.
fn type_message(import_type: ImportType) -> &'static str {
    match import_type {
        ImportType::Following => "import-type-following",
        ImportType::Blocking => "import-type-blocking",
        ImportType::Muting => "import-type-muting",
        ImportType::DomainBlocking => "import-type-domain-blocking",
        ImportType::Bookmarks => "import-type-bookmarks",
        ImportType::Lists => "import-type-lists",
    }
}

/// The same, for the stored column, which is only ever one of the six.
fn type_message_of(stored: &str) -> &'static str {
    ImportType::from_str(stored).map_or("import-type-unknown", type_message)
}

fn state_message(import: &BulkImport) -> &'static str {
    match import.state.as_str() {
        "unconfirmed" => "import-state-unconfirmed",
        "scheduled" => "import-state-scheduled",
        "in_progress" => "import-state-in-progress",
        "finished" => "import-state-finished",
        _ => "import-state-unknown",
    }
}

fn import_detail(import: &BulkImport, locale: Locale) -> String {
    let mut args = FluentArgs::new();
    match import.state.as_str() {
        "finished" => {
            args.set("imported", import.imported_items);
            args.set("total", import.total_items);
            args.set("failed", import.failure_count());
            locale.text_with("import-detail-finished", &args)
        }
        "in_progress" => {
            args.set("processed", import.processed_items);
            args.set("total", import.total_items);
            locale.text_with("import-detail-in-progress", &args)
        }
        _ => {
            args.set("rows", import.total_items);
            locale.text_with("import-detail-rows", &args)
        }
    }
}

fn error_code(error: &ParseError) -> &'static str {
    match error {
        ParseError::Empty => "empty",
        ParseError::Malformed => "malformed",
        ParseError::IncompatibleType => "incompatible",
        ParseError::TooManyRows => "too_many",
    }
}

/// Maps a redirect's `?error=` code onto catalog copy. An unknown code renders
/// nothing at all, so nothing a caller puts in the query string reaches the
/// page as text.
fn error_message(code: &str) -> Option<&'static str> {
    Some(match code {
        "empty" => "import-error-empty",
        "malformed" => "import-error-malformed",
        "incompatible" => "import-error-incompatible",
        "too_many" => "import-error-too-many",
        "too_many_pending" => "import-error-too-many-pending",
        "too_large" => "import-error-too-large",
        "server_busy" => "import-error-server-busy",
        "bad_type" => "import-error-bad-type",
        "archive_rate" => "archive-error-rate",
        _ => return None,
    })
}

/// The success flash for the page, differentiating an archive request
/// (`?saved=archive`) from a confirmed import.
fn saved_message(saved: Option<&str>) -> &'static str {
    match saved {
        Some("archive") => "archive-saved",
        _ => "import-saved",
    }
}

#[cfg(test)]
mod tests {
    use super::{FILE_SIZE_LIMIT, Locale};
    use crate::bulk_import::ROWS_LIMIT;

    /// The upload hint and the over-size/over-length refusals spell their
    /// limits out rather than interpolating them: Fluent renders numbers
    /// ungrouped ("20000"), and a translator writes the separator their locale
    /// uses. That leaves the copy free to drift from the constants, so pin it.
    #[test]
    fn the_stated_upload_limits_match_the_real_ones() {
        fn grouped(value: usize) -> String {
            let digits = value.to_string();
            let mut out = String::new();
            for (index, digit) in digits.chars().enumerate() {
                if index > 0 && (digits.len() - index).is_multiple_of(3) {
                    out.push(',');
                }
                out.push(digit);
            }
            out
        }

        let megabytes = format!("{} MB", FILE_SIZE_LIMIT / (1024 * 1024));
        let rows = format!("{} rows", grouped(ROWS_LIMIT));
        let english = Locale::default();
        for (id, expected) in [
            ("import-file-hint", &megabytes),
            ("import-file-hint", &rows),
            ("import-error-too-large", &megabytes),
            ("import-error-too-many", &rows),
        ] {
            let message = english.text(id);
            assert!(
                message.contains(expected.as_str()),
                "{id} should state {expected:?}, but reads {message:?}"
            );
        }
    }
}
