//! Web Push delivery — Mastodon's stack: RFC 8030 (the POST to the push
//! service), RFC 8291 content encryption (`aes128gcm`, plus the pre-RFC
//! `aesgcm` draft scheme for `standard: false` subscriptions, which is what
//! most native apps register), RFC 8292 VAPID server identification.
//!
//! The payload is Mastodon's `Web::NotificationSerializer` JSON; the worker
//! drains `push_delivery_jobs` (filled by the notifications fan-out trigger)
//! with the delivery-queue pattern, and prunes subscriptions the push
//! service reports dead (4xx other than 408/429).

use std::time::Duration;

use aes_gcm::aead::Aead;
use aes_gcm::{Aes128Gcm, KeyInit, Nonce};
use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use futures_util::stream::StreamExt;
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToSec1Point;
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use plamenu_db::web_push::{self, VapidKeys};
use plamenu_db::{account, notification, status, user};
use serde_json::json;
use sha2::Sha256;
use time::OffsetDateTime;
use tokio::task::JoinHandle;

use crate::AppState;
use crate::entities::avatar_url;
use crate::federation::WebPush;

const BATCH_SIZE: i64 = 20;
const IDLE_POLL: Duration = Duration::from_secs(1);
/// How many pushes in a claimed batch are delivered at once.
/// The batch is composed fairly across users by [`web_push::claim_due`], and
/// draining it with bounded concurrency — rather than the former strictly
/// serial loop — means one slow or hostile push endpoint no longer blocks the
/// whole batch behind it. The federation client keeps its own global outbound
/// cap, so this only bounds how many push POSTs this worker starts in parallel.
const SEND_CONCURRENCY: usize = 8;

/// Pushes expire after 48 hours (Mastodon's `PushNotificationWorker::TTL`),
/// both as the message's `Ttl` header and as the give-up bound for stale
/// queued notifications.
const TTL_SECONDS: i64 = 48 * 3600;
/// VAPID tokens are minted per delivery and live 24 hours, like Mastodon's.
const JWT_TTL_SECONDS: i64 = 24 * 3600;

/// The server's VAPID identity, loaded from (or generated into) the
/// one-row `vapid_keys` table.
pub struct Vapid {
    signing_key: SigningKey,
    /// Uncompressed SEC1 public point, base64url with padding — the form
    /// `server_key` and `configuration.vapid.public_key` serve.
    pub public_key: String,
}

impl Vapid {
    /// The unpadded form used inside `Authorization`/`Crypto-Key` headers
    /// (RFC 8292 requires unpadded base64url there).
    fn public_key_for_header(&self) -> String {
        self.public_key.trim_end_matches('=').to_owned()
    }
}

/// Loads the VAPID keypair, generating and persisting one on first use.
/// The keypair is permanent: push services bind subscriptions to it.
pub async fn vapid(state: &AppState) -> Result<Vapid, String> {
    let existing = web_push::vapid_get(&state.pool)
        .await
        .map_err(|e| e.to_string())?;
    let stored = if let Some(keys) = existing {
        keys
    } else {
        let secret = random_secret();
        let pem = secret
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|e| e.to_string())?
            .to_string();
        let public = URL_SAFE.encode(secret.public_key().to_sec1_point(false).as_bytes());
        web_push::vapid_create_if_missing(&state.pool, &pem, &public)
            .await
            .map_err(|e| e.to_string())?
    };
    from_stored(&stored)
}

fn from_stored(stored: &VapidKeys) -> Result<Vapid, String> {
    let secret = p256::SecretKey::from_pkcs8_pem(&stored.private_key).map_err(|e| e.to_string())?;
    Ok(Vapid {
        signing_key: SigningKey::from(&secret),
        public_key: stored.public_key.clone(),
    })
}

/// A fresh P-256 secret key from OS entropy.
fn random_secret() -> p256::SecretKey {
    loop {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("OS entropy source failed");
        // Rejected only for 0 or ≥ the group order — practically never.
        if let Ok(secret) = p256::SecretKey::from_slice(&bytes) {
            return secret;
        }
    }
}

/// The subscriber's keys, decoded from the registration request.
/// `None` means what Mastodon's `WebPushKeyValidator` calls an invalid key.
pub struct ClientKeys {
    ua_public: p256::PublicKey,
    auth: Vec<u8>,
}

/// Decodes base64url with or without padding (Ruby's `urlsafe_decode64`
/// accepts both, so subscription keys arrive in either form).
fn b64url(value: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(value.trim_end_matches('=')).ok()
}

#[must_use]
pub fn parse_client_keys(p256dh: &str, auth: &str) -> Option<ClientKeys> {
    let ua_public = p256::PublicKey::from_sec1_bytes(&b64url(p256dh)?).ok()?;
    let auth = b64url(auth)?;
    if auth.is_empty() {
        return None;
    }
    Some(ClientKeys { ua_public, auth })
}

fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8], okm: &mut [u8]) {
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info, okm)
        .expect("okm length is always valid for HKDF-SHA256");
}

/// RFC 8291 `aes128gcm`: a single record with the keying material carried
/// in the body's own header block.
fn encrypt_aes128gcm(
    keys: &ClientKeys,
    plaintext: &[u8],
    as_secret: &p256::SecretKey,
    salt: &[u8; 16],
) -> Result<Vec<u8>, String> {
    let as_public = as_secret.public_key().to_sec1_point(false);
    let ua_public = keys.ua_public.to_sec1_point(false);
    let shared =
        p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), keys.ua_public.as_affine());

    let mut key_info = b"WebPush: info\x00".to_vec();
    key_info.extend_from_slice(ua_public.as_bytes());
    key_info.extend_from_slice(as_public.as_bytes());
    let mut ikm = [0u8; 32];
    hkdf_sha256(&keys.auth, shared.raw_secret_bytes(), &key_info, &mut ikm);

    let mut cek = [0u8; 16];
    hkdf_sha256(salt, &ikm, b"Content-Encoding: aes128gcm\x00", &mut cek);
    let mut nonce = [0u8; 12];
    hkdf_sha256(salt, &ikm, b"Content-Encoding: nonce\x00", &mut nonce);

    // One record: the plaintext, a 0x02 last-record delimiter, no padding.
    let mut record = plaintext.to_vec();
    record.push(0x02);
    let ciphertext = Aes128Gcm::new_from_slice(&cek)
        .expect("cek is 16 bytes")
        .encrypt(
            &Nonce::try_from(&nonce[..]).expect("nonce is 12 bytes"),
            record.as_slice(),
        )
        .map_err(|e| e.to_string())?;

    // Header block: salt, record size, keyid = the ephemeral public key.
    let mut body = Vec::with_capacity(16 + 4 + 1 + 65 + ciphertext.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(65);
    body.extend_from_slice(as_public.as_bytes());
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

/// The pre-RFC `aesgcm` draft scheme (webpush-encryption-04): keying
/// material rides in `Encryption`/`Crypto-Key` headers instead of the body.
/// Returns the ciphertext body and the ephemeral public key for the header.
fn encrypt_aesgcm(
    keys: &ClientKeys,
    plaintext: &[u8],
    as_secret: &p256::SecretKey,
    salt: &[u8; 16],
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let as_public = as_secret.public_key().to_sec1_point(false);
    let ua_public = keys.ua_public.to_sec1_point(false);
    let shared =
        p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), keys.ua_public.as_affine());

    let mut ikm = [0u8; 32];
    hkdf_sha256(
        &keys.auth,
        shared.raw_secret_bytes(),
        b"Content-Encoding: auth\x00",
        &mut ikm,
    );

    // context = "P-256" || 0x00 || len16(ua_public) || ua_public
    //                            || len16(as_public) || as_public
    let mut context = b"P-256\x00".to_vec();
    context.extend_from_slice(&65u16.to_be_bytes());
    context.extend_from_slice(ua_public.as_bytes());
    context.extend_from_slice(&65u16.to_be_bytes());
    context.extend_from_slice(as_public.as_bytes());

    let mut cek_info = b"Content-Encoding: aesgcm\x00".to_vec();
    cek_info.extend_from_slice(&context);
    let mut nonce_info = b"Content-Encoding: nonce\x00".to_vec();
    nonce_info.extend_from_slice(&context);

    let mut cek = [0u8; 16];
    hkdf_sha256(salt, &ikm, &cek_info, &mut cek);
    let mut nonce = [0u8; 12];
    hkdf_sha256(salt, &ikm, &nonce_info, &mut nonce);

    // Two-byte big-endian padding length (zero), then the plaintext.
    let mut record = vec![0u8, 0u8];
    record.extend_from_slice(plaintext);
    let ciphertext = Aes128Gcm::new_from_slice(&cek)
        .expect("cek is 16 bytes")
        .encrypt(
            &Nonce::try_from(&nonce[..]).expect("nonce is 12 bytes"),
            record.as_slice(),
        )
        .map_err(|e| e.to_string())?;
    Ok((ciphertext, as_public.as_bytes().to_vec()))
}

/// The VAPID `aud` claim: the endpoint's origin, default ports elided
/// (Addressable's `normalized_site`).
fn audience(endpoint: &str) -> Option<String> {
    let uri: axum::http::Uri = endpoint.parse().ok()?;
    let scheme = uri.scheme_str()?;
    let host = uri.host()?;
    match uri.port_u16() {
        Some(port) if !matches!((scheme, port), ("https", 443) | ("http", 80)) => {
            Some(format!("{scheme}://{host}:{port}"))
        }
        _ => Some(format!("{scheme}://{host}")),
    }
}

/// A signed VAPID JWT (ES256) for `audience`.
fn vapid_jwt(vapid: &Vapid, audience: &str, contact: &str, now: OffsetDateTime) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "aud": audience,
            "exp": now.unix_timestamp() + JWT_TTL_SECONDS,
            "sub": contact,
        }))
        .expect("claims are serializable"),
    );
    let message = format!("{header}.{claims}");
    let signature: Signature = vapid.signing_key.sign(message.as_bytes());
    format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
}

/// Builds the complete RFC 8030 request for one notification payload,
/// per the subscription's scheme.
fn build_request(
    vapid: &Vapid,
    subscription: &web_push::Subscription,
    keys: &ClientKeys,
    payload: &[u8],
    contact: &str,
) -> Result<WebPush, String> {
    let aud = audience(&subscription.endpoint)
        .ok_or_else(|| format!("unparsable endpoint {}", subscription.endpoint))?;
    let jwt = vapid_jwt(vapid, &aud, contact, OffsetDateTime::now_utc());
    let as_secret = random_secret();
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).expect("OS entropy source failed");

    let mut headers = vec![
        ("Ttl".to_owned(), TTL_SECONDS.to_string()),
        ("Urgency".to_owned(), "normal".to_owned()),
    ];
    let body;
    if subscription.standard {
        body = encrypt_aes128gcm(keys, payload, &as_secret, &salt)?;
        headers.push(("Content-Encoding".to_owned(), "aes128gcm".to_owned()));
        headers.push((
            "Authorization".to_owned(),
            format!("vapid t={jwt},k={}", vapid.public_key_for_header()),
        ));
    } else {
        let (ciphertext, as_public) = encrypt_aesgcm(keys, payload, &as_secret, &salt)?;
        body = ciphertext;
        headers.push(("Content-Encoding".to_owned(), "aesgcm".to_owned()));
        headers.push((
            "Encryption".to_owned(),
            format!("salt={}", URL_SAFE_NO_PAD.encode(salt)),
        ));
        headers.push((
            "Crypto-Key".to_owned(),
            format!(
                "dh={};p256ecdsa={}",
                URL_SAFE_NO_PAD.encode(&as_public),
                vapid.public_key_for_header()
            ),
        ));
        headers.push(("Authorization".to_owned(), format!("WebPush {jwt}")));
    }
    Ok(WebPush {
        endpoint: subscription.endpoint.clone(),
        headers,
        body,
    })
}

/// The push title — `notification_mailer.{type}.subject` from Mastodon's
/// English locale. `None` for kinds Plamenu never emits.
fn title(kind: &str, name: &str) -> Option<String> {
    Some(match kind {
        "mention" => format!("You were mentioned by {name}"),
        "follow" => format!("{name} is now following you"),
        "follow_request" => format!("Pending follower: {name}"),
        "favourite" => format!("{name} favorited your post"),
        "reblog" => format!("{name} boosted your post"),
        "quote" => format!("{name} quoted your post"),
        "pleroma:emoji_reaction" => format!("{name} reacted to your post"),
        "update" => format!("{name} edited a post"),
        "quoted_update" => format!("{name} edited a post you have quoted"),
        "live" => format!("{name} is live now"),
        "poll" => format!("A poll by {name} has ended"),
        "status" => format!("{name} just posted"),
        "moderation_warning" => "Your account has received a moderation warning".to_owned(),
        // Event participation (E-track). Without an arm here `deliver` drops the
        // notification permanently ("kind has no push rendering"), so a kind added
        // to `notifications::TYPES` alone never reaches a subscriber's device.
        "event.participation" => format!("{name} responded to your event"),
        "event.accepted" => format!("{name} confirmed your attendance"),
        "event.rejected" => format!("{name} declined your attendance"),
        "event.changed" => format!("{name} changed an event you're attending"),
        "event.invite" => format!("{name} invited you to an event"),
        _ => return None,
    })
}

/// Rails `strip_tags`: drops every `<...>` run. Our stored HTML is
/// sanitized, so naive scanning is safe.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// The entity decoding Mastodon applies after `strip_tags`
/// (`HTMLEntities.new.decode`), over the entities sanitized HTML contains.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let Some(end) = tail.find(';').filter(|&e| e <= 32) else {
            out.push('&');
            rest = &rest[start + 1..];
            continue;
        };
        let entity = &tail[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| {
                    n.strip_prefix(['x', 'X']).map_or_else(
                        || n.parse::<u32>().ok(),
                        |h| u32::from_str_radix(h, 16).ok(),
                    )
                })
                .and_then(char::from_u32),
        };
        if let Some(c) = decoded {
            out.push(c);
            rest = &rest[start + end + 1..];
        } else {
            out.push('&');
            rest = &rest[start + 1..];
        }
    }
    out.push_str(rest);
    out
}

/// Rails `truncate(length: 140)`: hard cut with a `...` omission counted
/// inside the limit.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(limit - 3).collect();
    out.push_str("...");
    out
}

/// One push attempt's terminal classification.
enum AttemptError {
    /// Never retry (stale, unrenderable, or the subscription is gone).
    Permanent(String),
    /// Worth retrying with backoff.
    Transient(String),
}

async fn attempt(
    state: &AppState,
    vapid: &Vapid,
    job: &web_push::PushJob,
) -> Result<(), AttemptError> {
    let permanent = |reason: &str| AttemptError::Permanent(reason.to_owned());
    let db_err = |e: plamenu_db::DbError| AttemptError::Transient(e.to_string());

    let subscription = web_push::find_by_id(&state.pool, job.subscription_id)
        .await
        .map_err(db_err)?
        .ok_or_else(|| permanent("subscription vanished"))?;
    let notification = notification::find(&state.pool, job.notification_id)
        .await
        .map_err(db_err)?
        .ok_or_else(|| permanent("notification vanished"))?;

    // Stale notifications are pointless to wake a phone for.
    if OffsetDateTime::now_utc() - notification.created_at > time::Duration::seconds(TTL_SECONDS) {
        return Err(permanent("notification expired"));
    }
    // Re-check like Mastodon's worker: alerts/policy may have changed since
    // the fan-out trigger enqueued the job.
    if !web_push::pushable(
        &state.pool,
        &subscription,
        notification.account_id,
        notification.from_account_id,
        &notification.kind,
    )
    .await
    .map_err(db_err)?
    {
        return Err(permanent("no longer pushable"));
    }

    let Some(keys) = parse_client_keys(&subscription.key_p256dh, &subscription.key_auth) else {
        // Pre-validation rows can't ever be delivered; Mastodon's worker
        // destroys them too.
        let _ = web_push::delete(&state.pool, subscription.id).await;
        return Err(permanent("invalid client keys"));
    };

    let from_account = account::find_by_id(&state.pool, notification.from_account_id)
        .await
        .map_err(db_err)?
        .ok_or_else(|| permanent("sender vanished"))?;
    let target_status = match notification.status_id {
        Some(id) => status::find_by_id(&state.pool, id).await.map_err(db_err)?,
        None => None,
    };

    let domain = &state.config.domain;
    let name = if from_account.display_name.is_empty() {
        &from_account.username
    } else {
        &from_account.display_name
    };
    let title =
        title(&notification.kind, name).ok_or_else(|| permanent("kind has no push rendering"))?;
    let body_source = match &target_status {
        Some(status) if !status.spoiler_text.is_empty() => status.spoiler_text.clone(),
        Some(status) => status.content.clone(),
        None => from_account.note.clone(),
    };
    let body = truncate(&decode_entities(&strip_tags(&body_source)), 140);
    // Push icons route through the proxy; the last-resort direct fallback does
    // not apply to a service-worker-fetched notification icon.
    let icon = avatar_url(domain, &from_account, false)
        .unwrap_or_else(|| format!("https://{domain}/static/missing.png"));

    // Mastodon's `Web::NotificationSerializer` payload.
    let preferred_locale = user::locale_by_account_id(&state.pool, notification.account_id)
        .await
        .map_err(db_err)?
        .unwrap_or_else(|| "en".to_owned());
    let payload = serde_json::to_vec(&json!({
        "access_token": subscription.access_token,
        "preferred_locale": preferred_locale,
        "notification_id": notification.id,
        "notification_type": notification.kind,
        "icon": icon,
        "title": title,
        "body": body,
    }))
    .expect("payload is serializable");

    let contact = format!("mailto:admin@{domain}");
    let request = build_request(vapid, &subscription, &keys, &payload, &contact)
        .map_err(AttemptError::Permanent)?;
    let status_code = state
        .federation
        .web_push(request)
        .await
        .map_err(|e| AttemptError::Transient(e.to_string()))?;

    match status_code {
        200..=299 => Ok(()),
        // Rate limited or timed out: retry. Any other 4xx means the
        // subscription is invalid or expired and must be removed.
        408 | 429 => Err(AttemptError::Transient(format!(
            "push service answered {status_code}"
        ))),
        400..=499 => {
            let _ = web_push::delete(&state.pool, subscription.id).await;
            Err(permanent(&format!(
                "push service answered {status_code}; subscription removed"
            )))
        }
        _ => Err(AttemptError::Transient(format!(
            "push service answered {status_code}"
        ))),
    }
}

/// Claims and attempts one batch of due pushes; returns how many jobs were
/// claimed (0 = the queue is currently drained).
pub async fn run_due(state: &AppState) -> u64 {
    let jobs = match web_push::claim_due(&state.pool, BATCH_SIZE).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!(%error, "failed to claim push jobs");
            return 0;
        }
    };
    if jobs.is_empty() {
        return 0;
    }
    let vapid = match vapid(state).await {
        Ok(vapid) => vapid,
        Err(error) => {
            tracing::error!(%error, "failed to load VAPID keys");
            return 0;
        }
    };
    let claimed = jobs.len() as u64;
    let vapid = &vapid;
    futures_util::stream::iter(jobs)
        .for_each_concurrent(SEND_CONCURRENCY, |job| async move {
            match attempt(state, vapid, &job).await {
                Ok(()) => {
                    let _ = web_push::complete(&state.pool, job.id).await;
                }
                Err(AttemptError::Permanent(reason)) => {
                    tracing::debug!(%reason, "dropping push job");
                    let _ = web_push::complete(&state.pool, job.id).await;
                }
                Err(AttemptError::Transient(reason)) => {
                    if job.attempts >= web_push::MAX_ATTEMPTS {
                        tracing::warn!(%reason, attempts = job.attempts, "dropping undeliverable push");
                        let _ = web_push::complete(&state.pool, job.id).await;
                    } else {
                        tracing::debug!(%reason, attempts = job.attempts, "push failed; will retry");
                        let _ = web_push::retry_later(&state.pool, job.id, job.attempts).await;
                    }
                }
            }
        })
        .await;
    claimed
}

/// Runs the push delivery loop until the process exits.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("web push worker started");
        loop {
            if run_due(&state).await == 0 && !crate::workers::pause(&state, IDLE_POLL).await {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier;

    use super::*;

    #[test]
    fn rfc_8291_test_vector() {
        // RFC 8291 section 5: fixed keys and salt produce a fixed body.
        let as_secret = p256::SecretKey::from_slice(
            &b64url("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw").unwrap(),
        )
        .unwrap();
        let keys = parse_client_keys(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
            "BTBZMqHH6r4Tts7J_aSIgg",
        )
        .unwrap();
        let salt: [u8; 16] = b64url("DGv6ra1nlYgDCS1FRnbzlw")
            .unwrap()
            .try_into()
            .unwrap();
        let body = encrypt_aes128gcm(
            &keys,
            b"When I grow up, I want to be a watermelon",
            &as_secret,
            &salt,
        )
        .unwrap();
        assert_eq!(
            URL_SAFE_NO_PAD.encode(&body),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml\
             mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT\
             pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
    }

    /// Decrypts a legacy `aesgcm` message the way a push client would.
    fn decrypt_aesgcm(
        ua_secret: &p256::SecretKey,
        auth: &[u8],
        as_public: &[u8],
        salt: &[u8],
        ciphertext: &[u8],
    ) -> Vec<u8> {
        let as_public = p256::PublicKey::from_sec1_bytes(as_public).unwrap();
        let ua_public = ua_secret.public_key().to_sec1_point(false);
        let shared =
            p256::ecdh::diffie_hellman(ua_secret.to_nonzero_scalar(), as_public.as_affine());
        let mut ikm = [0u8; 32];
        hkdf_sha256(
            auth,
            shared.raw_secret_bytes(),
            b"Content-Encoding: auth\x00",
            &mut ikm,
        );
        let mut context = b"P-256\x00".to_vec();
        context.extend_from_slice(&65u16.to_be_bytes());
        context.extend_from_slice(ua_public.as_bytes());
        context.extend_from_slice(&65u16.to_be_bytes());
        context.extend_from_slice(as_public.to_sec1_point(false).as_bytes());
        let mut cek_info = b"Content-Encoding: aesgcm\x00".to_vec();
        cek_info.extend_from_slice(&context);
        let mut nonce_info = b"Content-Encoding: nonce\x00".to_vec();
        nonce_info.extend_from_slice(&context);
        let mut cek = [0u8; 16];
        hkdf_sha256(salt, &ikm, &cek_info, &mut cek);
        let mut nonce = [0u8; 12];
        hkdf_sha256(salt, &ikm, &nonce_info, &mut nonce);
        let padded = Aes128Gcm::new_from_slice(&cek)
            .unwrap()
            .decrypt(
                &Nonce::try_from(&nonce[..]).expect("nonce is 12 bytes"),
                ciphertext,
            )
            .unwrap();
        assert_eq!(&padded[..2], &[0, 0]);
        padded[2..].to_vec()
    }

    #[test]
    fn legacy_aesgcm_roundtrip() {
        let ua_secret = random_secret();
        let auth = b"0123456789abcdef";
        let p256dh = URL_SAFE_NO_PAD.encode(ua_secret.public_key().to_sec1_point(false).as_bytes());
        let keys = parse_client_keys(&p256dh, &URL_SAFE_NO_PAD.encode(auth)).unwrap();

        let as_secret = random_secret();
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).unwrap();
        let (ciphertext, as_public) =
            encrypt_aesgcm(&keys, b"hello push", &as_secret, &salt).unwrap();
        let plaintext = decrypt_aesgcm(&ua_secret, auth, &as_public, &salt, &ciphertext);
        assert_eq!(plaintext, b"hello push");
    }

    #[test]
    fn vapid_jwt_signs_verifiable_claims() {
        let secret = random_secret();
        let vapid = Vapid {
            signing_key: SigningKey::from(&secret),
            public_key: URL_SAFE.encode(secret.public_key().to_sec1_point(false).as_bytes()),
        };
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let jwt = vapid_jwt(
            &vapid,
            "https://push.example",
            "mailto:admin@plamenu.test",
            now,
        );

        let [header, claims, signature]: [&str; 3] =
            jwt.split('.').collect::<Vec<_>>().try_into().unwrap();
        let decoded: serde_json::Value = serde_json::from_slice(&b64url(header).unwrap()).unwrap();
        assert_eq!(decoded["alg"], "ES256");
        let decoded: serde_json::Value = serde_json::from_slice(&b64url(claims).unwrap()).unwrap();
        assert_eq!(decoded["aud"], "https://push.example");
        assert_eq!(decoded["sub"], "mailto:admin@plamenu.test");
        assert_eq!(decoded["exp"], json!(1_700_000_000 + JWT_TTL_SECONDS));

        let verifying = VerifyingKey::from(&vapid.signing_key);
        let signature = Signature::from_slice(&b64url(signature).unwrap()).unwrap();
        let message = jwt.rsplit_once('.').unwrap().0;
        verifying.verify(message.as_bytes(), &signature).unwrap();
    }

    #[test]
    fn audience_elides_default_ports() {
        assert_eq!(
            audience("https://push.example/wpush/abc").unwrap(),
            "https://push.example"
        );
        assert_eq!(
            audience("https://push.example:443/x").unwrap(),
            "https://push.example"
        );
        assert_eq!(
            audience("https://push.example:8443/x").unwrap(),
            "https://push.example:8443"
        );
        assert!(audience("not a url").is_none());
    }

    #[test]
    fn body_text_pipeline_matches_rails() {
        let html = "<p>Hello <a href=\"https://x.example\">&amp;world&#39;s</a><br>next</p>";
        assert_eq!(decode_entities(&strip_tags(html)), "Hello &world'snext");
        assert_eq!(truncate("abc", 140), "abc");
        let long = "x".repeat(150);
        let cut = truncate(&long, 140);
        assert_eq!(cut.chars().count(), 140);
        assert!(cut.ends_with("..."));
    }

    #[test]
    fn titles_match_mastodon_subjects() {
        assert_eq!(
            title("favourite", "Alice").unwrap(),
            "Alice favorited your post"
        );
        assert_eq!(
            title("follow_request", "Alice").unwrap(),
            "Pending follower: Alice"
        );
        assert!(title("severed_relationships", "Alice").is_none());
    }

    /// Every event notification kind must have a push rendering.
    ///
    /// `deliver` treats a missing title as a *permanent* failure ("kind has no
    /// push rendering"), so a kind registered in `notifications::TYPES` but not
    /// here is silently never pushed to anyone. The E-track shipped with exactly
    /// that gap, which is why this is pinned rather than left to review.
    #[test]
    fn every_event_kind_has_a_push_title() {
        for kind in [
            crate::events::NOTIFY_PARTICIPATION,
            crate::events::NOTIFY_ACCEPTED,
            crate::events::NOTIFY_REJECTED,
            crate::events::NOTIFY_CHANGED,
            crate::events::NOTIFY_INVITED,
        ] {
            assert!(
                title(kind, "Alice").is_some(),
                "{kind} would be dropped from web push"
            );
            assert!(
                crate::routes::notifications::TYPES.contains(&kind),
                "{kind} is not in the notification type allowlist"
            );
        }
    }
}
