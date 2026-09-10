//! Webhook dispatch — Mastodon's `WebhookService` plus
//! `Webhooks::DeliveryWorker`.
//!
//! Trigger functions serialize the event payload once — `{event, created_at,
//! object}` with the object as its `Admin::Account` / `Admin::Report` /
//! `Status` entity, Mastodon's `WebhookEventSerializer` — and enqueue one
//! `webhook_delivery_jobs` row per enabled webhook subscribed to the event.
//! The worker renders the webhook's optional `{{path}}` template, signs the
//! body into `X-Hub-Signature: sha256=<hmac>` and POSTs it, retrying
//! transient failures with the delivery-queue backoff. Triggers are
//! best-effort: a failure is logged, never surfaced to the user action that
//! fired it.

use std::time::Duration;

use plamenu_db::report::Report;
use plamenu_db::status::Status;
use plamenu_db::{admin_account, webhook};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;

use crate::AppState;
use crate::entities::{admin_account_json, admin_report_json, render_status, rfc3339};
use crate::error::ApiError;
use crate::federation::WebhookPost;
use crate::followers_sync::hex;

const BATCH_SIZE: i64 = 20;
const IDLE_POLL: Duration = Duration::from_secs(1);

/// Fires an `account.*` event for a local account.
pub async fn account_event(state: &AppState, event: &str, account_id: i64) {
    if let Err(error) = try_account_event(state, event, account_id).await {
        tracing::warn!(error = %error.chain(), event, "webhook trigger failed");
    }
}

async fn try_account_event(state: &AppState, event: &str, account_id: i64) -> Result<(), ApiError> {
    let hooks = webhook::enabled_for_event(&state.pool, event).await?;
    if hooks.is_empty() {
        return Ok(());
    }
    let view = admin_account::show(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let object = admin_account_json(&state.pool, &state.config.domain, &view).await?;
    enqueue(state, &hooks, event, object).await
}

/// Fires a `report.*` event.
pub async fn report_event(state: &AppState, event: &str, report: &Report) {
    if let Err(error) = try_report_event(state, event, report).await {
        tracing::warn!(error = %error.chain(), event, "webhook trigger failed");
    }
}

async fn try_report_event(state: &AppState, event: &str, report: &Report) -> Result<(), ApiError> {
    let hooks = webhook::enabled_for_event(&state.pool, event).await?;
    if hooks.is_empty() {
        return Ok(());
    }
    let object = admin_report_json(&state.pool, &state.config.domain, report).await?;
    enqueue(state, &hooks, event, object).await
}

/// Fires a `status.*` event for a local status.
pub async fn status_event(state: &AppState, event: &str, status: &Status) {
    if let Err(error) = try_status_event(state, event, status).await {
        tracing::warn!(error = %error.chain(), event, "webhook trigger failed");
    }
}

async fn try_status_event(state: &AppState, event: &str, status: &Status) -> Result<(), ApiError> {
    let hooks = webhook::enabled_for_event(&state.pool, event).await?;
    if hooks.is_empty() {
        return Ok(());
    }
    // Serialized without a viewer, like Mastodon's `scope: nil` render.
    let object = render_status(&state.pool, &state.config.domain, status, None).await?;
    enqueue(state, &hooks, event, object).await
}

async fn enqueue(
    state: &AppState,
    hooks: &[webhook::Webhook],
    event: &str,
    object: Value,
) -> Result<(), ApiError> {
    let body = json!({
        "event": event,
        "created_at": rfc3339(time::OffsetDateTime::now_utc())?,
        "object": object,
    })
    .to_string();
    for hook in hooks {
        webhook::enqueue_delivery(&state.pool, hook.id, event, &body).await?;
    }
    Ok(())
}

/// Claims and attempts one batch of due webhook deliveries; returns how many
/// jobs were claimed (0 = the queue is currently drained).
pub async fn run_due(state: &AppState) -> u64 {
    let jobs = match webhook::claim_due(&state.pool, BATCH_SIZE).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!(%error, "failed to claim webhook jobs");
            return 0;
        }
    };
    let claimed = jobs.len() as u64;
    for job in jobs {
        process(state, &job).await;
    }
    claimed
}

async fn process(state: &AppState, job: &webhook::DeliveryJob) {
    let hook = match webhook::find_by_id(&state.pool, job.webhook_id).await {
        Ok(Some(hook)) => hook,
        // Deleted mid-flight: drop the delivery, like Mastodon's rescued
        // `RecordNotFound`. (Disabling only stops new triggers; queued
        // deliveries still go out, also like Mastodon.)
        Ok(None) => {
            let _ = webhook::complete_delivery(&state.pool, job.id).await;
            return;
        }
        // The lease makes the job due again once it expires.
        Err(error) => {
            tracing::error!(%error, "failed to load webhook for delivery");
            return;
        }
    };
    let body = match hook.template.as_deref().filter(|t| !t.trim().is_empty()) {
        Some(template) => render_template(template, &job.body),
        None => job.body.clone(),
    };
    let signature = hex(&hmac_sha256(hook.secret.as_bytes(), body.as_bytes()));
    let post = WebhookPost {
        url: hook.url.clone(),
        headers: vec![("X-Hub-Signature".to_owned(), format!("sha256={signature}"))],
        body,
    };
    match state.federation.webhook(post).await {
        Ok(status) if (200..300).contains(&status) => {
            let _ = webhook::complete_delivery(&state.pool, job.id).await;
        }
        // Mastodon's `response_error_unsalvageable?`: retrying cannot help.
        Ok(status) if status == 501 || is_unsalvageable_4xx(status) => {
            tracing::debug!(status, webhook_id = hook.id, "webhook rejected; dropping");
            let _ = webhook::complete_delivery(&state.pool, job.id).await;
        }
        Ok(status) => retry(state, job, hook.id, &format!("HTTP {status}")).await,
        Err(error) => retry(state, job, hook.id, &error.to_string()).await,
    }
}

fn is_unsalvageable_4xx(status: u16) -> bool {
    (400..500).contains(&status) && !matches!(status, 401 | 408 | 429)
}

async fn retry(state: &AppState, job: &webhook::DeliveryJob, webhook_id: i64, reason: &str) {
    if job.attempts >= webhook::MAX_DELIVERY_ATTEMPTS {
        tracing::warn!(
            reason,
            webhook_id,
            attempts = job.attempts,
            "dropping undeliverable webhook"
        );
        let _ = webhook::complete_delivery(&state.pool, job.id).await;
    } else {
        tracing::debug!(
            reason,
            webhook_id,
            attempts = job.attempts,
            "webhook failed; will retry"
        );
        let _ = webhook::retry_delivery_later(&state.pool, job.id, job.attempts).await;
    }
}

#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("webhook worker started");
        loop {
            if run_due(&state).await == 0 && !crate::workers::pause(&state, IDLE_POLL).await {
                return;
            }
        }
    })
}

/// Renders Mastodon's payload template (`Webhooks::PayloadRenderer`):
/// `{{path.to.value}}` expressions are replaced with values dug out of the
/// payload JSON — strings are inserted JSON-escaped but unquoted so they can
/// be embedded inside other strings, everything else as compact JSON, and a
/// missing path yields `null`. Text not matching the strict expression
/// syntax is left verbatim.
fn render_template(template: &str, payload: &str) -> String {
    let Ok(document) = serde_json::from_str::<Value>(payload) else {
        return template.to_owned();
    };
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };
        if valid_path(&after[..end]) {
            out.push_str(&rest[..start]);
            out.push_str(&lookup(&document, &after[..end]));
            rest = &after[end + 2..];
        } else {
            // Not an expression — emit the braces and rescan right after
            // them, so `{{{{event}}` still finds the inner expression.
            out.push_str(&rest[..start + 2]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// `property(.property|.index)*` — the template parser's path grammar.
fn valid_path(expr: &str) -> bool {
    let mut segments = expr.split('.');
    segments.next().is_some_and(is_property)
        && segments.all(|segment| is_property(segment) || is_index(segment))
}

fn is_property(segment: &str) -> bool {
    !segment.is_empty() && segment.chars().all(|c| c.is_ascii_alphabetic() || c == '_')
}

fn is_index(segment: &str) -> bool {
    !segment.is_empty() && segment.chars().all(|c| c.is_ascii_digit())
}

fn lookup(document: &Value, path: &str) -> String {
    let mut current = document;
    for segment in path.split('.') {
        let next = match segment.parse::<usize>() {
            Ok(index) => current.get(index),
            Err(_) => current.get(segment),
        };
        let Some(value) = next else {
            return "null".to_owned();
        };
        current = value;
    }
    match current {
        // The JSON-escaped body of the string, without the quotes.
        Value::String(_) => {
            let quoted = current.to_string();
            quoted[1..quoted.len() - 1].to_owned()
        }
        other => other.to_string(),
    }
}

/// HMAC-SHA256 (RFC 2104) over `message` — the `X-Hub-Signature` digest.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(key_block.map(|b| b ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(key_block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_rfc_4231_vectors() {
        // Test case 2: short key, short data.
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 3: 20-byte 0xaa key, 50 bytes of 0xdd.
        assert_eq!(
            hex(&hmac_sha256(&[0xaa; 20], &[0xdd; 50])),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
        // Test case 6: key longer than the block size.
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn template_renders_paths_strings_and_json() {
        let payload = r#"{
            "event": "account.approved",
            "object": {
                "username": "foofoobarbar",
                "account": {"display_name": "Foo\""},
                "confirmed": true,
                "ids": [7, 9]
            }
        }"#;
        assert_eq!(
            render_template("foo={{event}}", payload),
            "foo=account.approved"
        );
        assert_eq!(
            render_template("foo={{object.username}}", payload),
            "foo=foofoobarbar"
        );
        // Strings are JSON-escaped but unquoted (Mastodon's renderer spec).
        assert_eq!(
            render_template("foo={{object.account.display_name}}", payload),
            r#"foo=Foo\""#
        );
        // Non-strings render as compact JSON; array indices dig.
        assert_eq!(
            render_template(
                r#"{"ok":{{object.confirmed}},"id":{{object.ids.1}}}"#,
                payload
            ),
            r#"{"ok":true,"id":9}"#
        );
        // A missing path is null.
        assert_eq!(render_template("{{object.nope}}", payload), "null");
    }

    #[test]
    fn template_leaves_non_expressions_verbatim() {
        let payload = r#"{"event":"status.created"}"#;
        assert_eq!(
            render_template("{{ event }} {{event", payload),
            "{{ event }} {{event"
        );
        assert_eq!(render_template("{{{{event}}", payload), "{{status.created");
        assert_eq!(render_template("{{9lives}}", payload), "{{9lives}}");
    }
}
