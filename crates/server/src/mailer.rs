//! Outgoing e-mail (M21) — Mastodon's `UserMailer` + sidekiq `mailers` queue.
//!
//! Flows render their message at trigger time and enqueue an `email_jobs`
//! row; the worker claims due rows, builds an RFC 5322 message and hands it
//! to the SMTP relay (`[smtp]` config, [`crate::config::SmtpConfig`]),
//! retrying transient failures with the delivery-queue backoff. With no
//! `[smtp]` section configured ([`enabled`] is false) the flows that need mail
//! must refuse upfront instead of enqueueing.

use std::time::Duration;

use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use plamenu_db::email::{self, EmailJob};
use tokio::task::JoinHandle;

use crate::AppState;
use crate::config::{SmtpConfig, Starttls};
use crate::error::ApiError;

const BATCH_SIZE: i64 = 20;
const IDLE_POLL: Duration = Duration::from_secs(1);

/// Whether outgoing e-mail is configured at all.
#[must_use]
pub fn enabled(state: &AppState) -> bool {
    state.config.smtp.is_some()
}

/// Queues one plain-text message. The caller has already checked
/// [`enabled`]; enqueueing without a configured relay still succeeds (the
/// row waits until a relay exists).
pub async fn enqueue(
    state: &AppState,
    recipient: &str,
    subject: &str,
    body: &str,
) -> Result<(), ApiError> {
    email::enqueue(&state.pool, recipient, subject, body).await?;
    Ok(())
}

/// The connected SMTP relay; built once at startup from [`SmtpConfig`].
#[derive(Clone)]
pub struct Smtp {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from_address: String,
}

impl Smtp {
    pub fn from_config(config: &SmtpConfig) -> Result<Self, lettre::transport::smtp::Error> {
        let tls_parameters = TlsParameters::new(config.server.clone())?;
        let tls = if config.ssl {
            Tls::Wrapper(tls_parameters)
        } else {
            match config.starttls {
                Starttls::Always => Tls::Required(tls_parameters),
                Starttls::Auto => Tls::Opportunistic(tls_parameters),
                Starttls::Never => Tls::None,
            }
        };
        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.server)
            .port(config.port)
            .tls(tls);
        if let (Some(login), Some(password)) = (&config.login, &config.password) {
            builder = builder.credentials(Credentials::new(login.clone(), password.clone()));
        }
        Ok(Self {
            transport: builder.build(),
            from_address: config.from_address.clone(),
        })
    }
}

/// One immediate, bounded send outside the queue — the admin console's
/// "send a test e-mail" button (O4). Unlike [`enqueue`], the relay's actual
/// verdict is returned to the caller instead of being retried in the
/// background: the whole point is surfacing misconfiguration.
pub async fn send_test(config: &SmtpConfig, recipient: &str) -> Result<(), String> {
    const TEST_SEND_TIMEOUT: Duration = Duration::from_secs(15);
    let smtp = Smtp::from_config(config).map_err(|error| error.to_string())?;
    let message = Message::builder()
        .from(
            smtp.from_address
                .parse()
                .map_err(|error| format!("invalid from_address: {error}"))?,
        )
        .to(recipient
            .parse()
            .map_err(|error| format!("invalid recipient: {error}"))?)
        .subject("Plamenu test e-mail")
        .header(ContentType::TEXT_PLAIN)
        .body(
            "This is the test message from the admin console.\n\n\
             If you are reading it, the SMTP relay works.\n"
                .to_owned(),
        )
        .map_err(|error| error.to_string())?;
    match tokio::time::timeout(TEST_SEND_TIMEOUT, smtp.transport.send(message)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err(format!(
            "the relay did not answer within {} seconds",
            TEST_SEND_TIMEOUT.as_secs()
        )),
    }
}

/// Claims and attempts one batch of due messages; returns how many jobs were
/// claimed (0 = the queue is currently drained).
pub async fn run_due(state: &AppState, smtp: &Smtp) -> u64 {
    let jobs = match email::claim_due(&state.pool, BATCH_SIZE).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!(%error, "failed to claim e-mail jobs");
            return 0;
        }
    };
    let claimed = jobs.len() as u64;
    for job in jobs {
        process(state, smtp, &job).await;
    }
    claimed
}

async fn process(state: &AppState, smtp: &Smtp, job: &EmailJob) {
    let message = match build_message(&smtp.from_address, job) {
        Ok(message) => message,
        // A malformed address or body cannot become sendable by retrying.
        Err(error) => {
            tracing::warn!(%error, recipient = %job.recipient, "dropping unbuildable e-mail");
            let _ = email::complete(&state.pool, job.id).await;
            return;
        }
    };
    match smtp.transport.send(message).await {
        Ok(_) => {
            let _ = email::complete(&state.pool, job.id).await;
        }
        // A permanent (5xx) SMTP rejection: retrying cannot help.
        Err(error) if error.is_permanent() => {
            tracing::warn!(%error, recipient = %job.recipient, "e-mail rejected; dropping");
            let _ = email::complete(&state.pool, job.id).await;
        }
        Err(error) => retry(state, job, &error.to_string()).await,
    }
}

fn build_message(
    from_address: &str,
    job: &EmailJob,
) -> Result<Message, Box<dyn std::error::Error + Send + Sync>> {
    Ok(Message::builder()
        .from(from_address.parse()?)
        .to(job.recipient.parse()?)
        .subject(&job.subject)
        .header(ContentType::TEXT_PLAIN)
        .body(job.body.clone())?)
}

async fn retry(state: &AppState, job: &EmailJob, reason: &str) {
    if job.attempts >= email::MAX_SEND_ATTEMPTS {
        tracing::warn!(
            reason,
            recipient = %job.recipient,
            attempts = job.attempts,
            "dropping undeliverable e-mail"
        );
        let _ = email::complete(&state.pool, job.id).await;
    } else {
        tracing::debug!(
            reason,
            recipient = %job.recipient,
            attempts = job.attempts,
            "e-mail send failed; will retry"
        );
        let _ = email::retry_later(&state.pool, job.id, job.attempts).await;
    }
}

/// Spawns the send worker. Call only with SMTP configured; without it the
/// queue just accumulates (and flows should have refused to enqueue).
#[must_use]
pub fn spawn(state: AppState, smtp: Smtp) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("mailer worker started");
        loop {
            if run_due(&state, &smtp).await == 0 && !crate::workers::pause(&state, IDLE_POLL).await
            {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(recipient: &str) -> EmailJob {
        EmailJob {
            id: 1,
            recipient: recipient.to_owned(),
            subject: "Confirm your registration".to_owned(),
            body: "Hi!\n\nOpen https://example.com/confirm?token=abc to finish.\n".to_owned(),
            attempts: 1,
        }
    }

    #[test]
    fn builds_a_plain_text_message() {
        let message = build_message(
            "Plamenu <notifications@plamenu.local>",
            &job("v@example.com"),
        )
        .unwrap();
        let raw = String::from_utf8(message.formatted()).unwrap();
        assert!(raw.contains("From: Plamenu <notifications@plamenu.local>"));
        assert!(raw.contains("To: v@example.com"));
        assert!(raw.contains("Subject: Confirm your registration"));
        assert!(raw.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(raw.contains("https://example.com/confirm?token=abc"));
    }

    #[test]
    fn rejects_a_malformed_recipient() {
        assert!(build_message("notifications@plamenu.local", &job("not an address")).is_err());
    }
}
