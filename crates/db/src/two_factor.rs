//! Second-factor state: pending login challenges and one-time
//! recovery codes. Secrets never land here in the clear — challenge tokens
//! and recovery codes are stored as SHA-256 hashes.

use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// How long a password-verified login may wait for its second factor.
pub const CHALLENGE_TTL_SECONDS: i32 = 5 * 60;

/// Wrong codes tolerated before the challenge is invalidated (Mastodon
/// rate-limits second-factor attempts at 25/h per user).
pub const CHALLENGE_MAX_ATTEMPTS: i32 = 25;

/// How many recovery codes a (re)generation issues.
pub const BACKUP_CODE_COUNT: usize = 10;

/// The OAuth authorization request a second-factor challenge is bound to (QC
/// audit #34): the client, callback, granted scope, state, and PKCE challenge
/// the user reviewed at password time. The second-factor legs mint the grant
/// from these server-held values, never from a resubmitted form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OauthChallengeRequest {
    pub client_id: String,
    pub redirect_uri: String,
    pub scope: String,
    pub state: Option<String>,
    pub code_challenge: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Challenge {
    pub id: i64,
    pub user_id: i64,
    pub context: String,
    pub attempts: i32,
    pub webauthn_state: Option<Value>,
    /// The bound authorization request; `None` for non-OAuth contexts.
    pub oauth_request: Option<OauthChallengeRequest>,
    pub expires_at: OffsetDateTime,
}

/// Creates a pending challenge, sweeping expired rows while at it (they are
/// tiny and short-lived; no dedicated cleanup job needed). `oauth_request`
/// binds the challenge to the authorization request it defers;
/// pass `None` outside the OAuth flow. Returns the new challenge id so a
/// caller can attach `WebAuthn` ceremony state to it.
pub async fn create_challenge(
    pool: &PgPool,
    token_hash: &str,
    user_id: i64,
    context: &str,
    oauth_request: Option<&OauthChallengeRequest>,
) -> Result<i64, DbError> {
    let id = id::next();
    let mut tx = pool.begin().await?;
    sqlx::query!("DELETE FROM two_factor_challenges WHERE expires_at < now()")
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "INSERT INTO two_factor_challenges
             (id, token_hash, user_id, context,
              oauth_client_id, oauth_redirect_uri, oauth_scope, oauth_state,
              oauth_code_challenge, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9,
                 now() + ($10::int * interval '1 second'))",
        id,
        token_hash,
        user_id,
        context,
        oauth_request.map(|request| request.client_id.as_str()),
        oauth_request.map(|request| request.redirect_uri.as_str()),
        oauth_request.map(|request| request.scope.as_str()),
        oauth_request.and_then(|request| request.state.as_deref()),
        oauth_request.and_then(|request| request.code_challenge.as_deref()),
        CHALLENGE_TTL_SECONDS,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// A live (unexpired, matching-context) challenge by its token hash.
pub async fn find_challenge(
    pool: &PgPool,
    token_hash: &str,
    context: &str,
) -> Result<Option<Challenge>, DbError> {
    let row = sqlx::query!(
        r#"
        SELECT id, user_id, context, attempts, webauthn_state,
               oauth_client_id, oauth_redirect_uri, oauth_scope, oauth_state,
               oauth_code_challenge, expires_at
        FROM two_factor_challenges
        WHERE token_hash = $1 AND context = $2 AND expires_at > now()
        "#,
        token_hash,
        context,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| {
        // `oauth_client_id` is the presence marker; the NOT NULL trio is
        // written together in `create_challenge`.
        let oauth_request = match (row.oauth_client_id, row.oauth_redirect_uri, row.oauth_scope) {
            (Some(client_id), Some(redirect_uri), Some(scope)) => Some(OauthChallengeRequest {
                client_id,
                redirect_uri,
                scope,
                state: row.oauth_state,
                code_challenge: row.oauth_code_challenge,
            }),
            _ => None,
        };
        Challenge {
            id: row.id,
            user_id: row.user_id,
            context: row.context,
            attempts: row.attempts,
            webauthn_state: row.webauthn_state,
            oauth_request,
            expires_at: row.expires_at,
        }
    }))
}

/// Counts a wrong code against the challenge; at
/// [`CHALLENGE_MAX_ATTEMPTS`] the row is deleted and `false` comes back —
/// the user has to sign in again.
pub async fn record_challenge_attempt(pool: &PgPool, id: i64) -> Result<bool, DbError> {
    let attempts = sqlx::query_scalar!(
        "UPDATE two_factor_challenges SET attempts = attempts + 1
         WHERE id = $1
         RETURNING attempts",
        id,
    )
    .fetch_optional(pool)
    .await?;
    match attempts {
        Some(attempts) if attempts >= CHALLENGE_MAX_ATTEMPTS => {
            delete_challenge(pool, id).await?;
            Ok(false)
        }
        Some(_) => Ok(true),
        None => Ok(false),
    }
}

/// Consumes the challenge (successful second factor, or invalidation).
pub async fn delete_challenge(pool: &PgPool, id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM two_factor_challenges WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Atomically consumes the challenge as a single-use claim: deletes the row and
/// reports whether *this* call was the one that removed it. Two concurrent
/// second-factor completions of the same challenge both verify against the same
/// stored ceremony state, so only the request that wins this claim (`true`) may
/// mint a session/grant; the loser sees `false` and is rejected. This closes the
/// `WebAuthn` assertion replay window (finding #35), and mirrors the one-use
/// [`consume_backup_code`] pattern.
pub async fn consume_challenge(pool: &PgPool, id: i64) -> Result<bool, DbError> {
    let deleted = sqlx::query!("DELETE FROM two_factor_challenges WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() == 1)
}

/// [`consume_challenge`] and the signing key's counter update as ONE
/// transaction: the single-use claim and the clone-detection
/// state (bumped sign counter + serialized credential) commit together. A
/// completion can no longer win a session while best-effort-losing the counter
/// bump, and a failed credential write rolls the claim back so the user simply
/// retries against a still-live challenge. Returns whether *this* call won the
/// claim.
pub async fn consume_challenge_updating_credential(
    pool: &PgPool,
    challenge_id: i64,
    credential_id: i64,
    credential: &Value,
    sign_count: i64,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let deleted = sqlx::query!(
        "DELETE FROM two_factor_challenges WHERE id = $1",
        challenge_id
    )
    .execute(&mut *tx)
    .await?;
    if deleted.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(false);
    }
    sqlx::query!(
        "UPDATE webauthn_credentials SET credential = $2, sign_count = $3 WHERE id = $1",
        credential_id,
        credential,
        sign_count,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Stores serialized `WebAuthn` ceremony state on the challenge.
pub async fn set_webauthn_state(pool: &PgPool, id: i64, state: &Value) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE two_factor_challenges SET webauthn_state = $2 WHERE id = $1",
        id,
        state,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Replaces the user's recovery codes with a fresh set of hashes.
pub async fn replace_backup_codes(
    pool: &PgPool,
    user_id: i64,
    code_hashes: &[String],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query!("DELETE FROM otp_backup_codes WHERE user_id = $1", user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "INSERT INTO otp_backup_codes (user_id, code_hash)
         SELECT $1, unnest($2::text[])",
        user_id,
        code_hashes,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The outcome of an atomic TOTP enrollment ([`confirm_otp_enrollment`]).
#[derive(Debug, PartialEq, Eq)]
pub enum OtpEnrollment {
    /// 2FA is now on and fresh recovery codes are stored (show them once).
    Enabled,
    /// The confirming timestep was already consumed — the code can't be
    /// replayed as both a confirmation and a login.
    CodeReused,
    /// No provisional secret to enable (the setup step was skipped or lost).
    NoSecret,
}

/// Commits a TOTP enrollment as one transaction: claims the
/// confirming timestep (so the same code cannot double as a login code), flips
/// the login requirement on, and replaces the recovery codes — all or nothing.
/// A bare claim-then-enable-then-replace sequence can leave 2FA enabled with no
/// freshly shown recovery codes if the final step fails; here a failure rolls
/// the enablement (and the timestep claim) back, so the user simply retries. The
/// caller shows the plaintext recovery codes only after this returns
/// [`OtpEnrollment::Enabled`].
pub async fn confirm_otp_enrollment(
    pool: &PgPool,
    user_id: i64,
    timestep: i64,
    code_hashes: &[String],
) -> Result<OtpEnrollment, DbError> {
    let mut tx = pool.begin().await?;
    // Claim the timestep; a rolled-back transaction leaves it unconsumed.
    let claimed = sqlx::query!(
        "UPDATE users SET otp_consumed_timestep = $2
         WHERE id = $1
           AND (otp_consumed_timestep IS NULL OR otp_consumed_timestep < $2)",
        user_id,
        timestep,
    )
    .execute(&mut *tx)
    .await?;
    if claimed.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(OtpEnrollment::CodeReused);
    }
    // Enable the login requirement only if a provisional secret exists.
    let enabled = sqlx::query!(
        "UPDATE users SET otp_required_for_login = true
         WHERE id = $1 AND otp_secret IS NOT NULL",
        user_id,
    )
    .execute(&mut *tx)
    .await?;
    if enabled.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(OtpEnrollment::NoSecret);
    }
    sqlx::query!("DELETE FROM otp_backup_codes WHERE user_id = $1", user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "INSERT INTO otp_backup_codes (user_id, code_hash)
         SELECT $1, unnest($2::text[])",
        user_id,
        code_hashes,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(OtpEnrollment::Enabled)
}

/// One-time use: deleting the row is the consumption, so a code can never
/// pass twice.
pub async fn consume_backup_code(
    pool: &PgPool,
    user_id: i64,
    code_hash: &str,
) -> Result<bool, DbError> {
    let deleted = sqlx::query!(
        "DELETE FROM otp_backup_codes WHERE user_id = $1 AND code_hash = $2",
        user_id,
        code_hash,
    )
    .execute(pool)
    .await?;
    Ok(deleted.rows_affected() == 1)
}

/// How many unused recovery codes remain (shown on the settings page).
pub async fn backup_codes_remaining(pool: &PgPool, user_id: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM otp_backup_codes WHERE user_id = $1"#,
        user_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};

    async fn seed_user(pool: &PgPool) -> i64 {
        let account = account::create_local(
            pool,
            NewLocalAccount {
                username: "alice",
                display_name: "alice",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        crate::user::create(pool, account.id, Some("alice@example.com"), "hash")
            .await
            .unwrap()
            .id
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn challenge_lifecycle(pool: PgPool) {
        let user_id = seed_user(&pool).await;
        create_challenge(&pool, "hash-1", user_id, "web", None)
            .await
            .unwrap();

        let challenge = find_challenge(&pool, "hash-1", "web")
            .await
            .unwrap()
            .expect("live challenge");
        assert_eq!(challenge.user_id, user_id);
        assert_eq!(challenge.attempts, 0);
        // The wrong context does not resolve it.
        assert!(
            find_challenge(&pool, "hash-1", "oauth")
                .await
                .unwrap()
                .is_none()
        );

        assert!(record_challenge_attempt(&pool, challenge.id).await.unwrap());
        delete_challenge(&pool, challenge.id).await.unwrap();
        assert!(
            find_challenge(&pool, "hash-1", "web")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn challenge_attempt_cap_invalidates(pool: PgPool) {
        let user_id = seed_user(&pool).await;
        create_challenge(&pool, "hash-1", user_id, "web", None)
            .await
            .unwrap();
        let challenge = find_challenge(&pool, "hash-1", "web")
            .await
            .unwrap()
            .unwrap();
        for _ in 0..(CHALLENGE_MAX_ATTEMPTS - 1) {
            assert!(record_challenge_attempt(&pool, challenge.id).await.unwrap());
        }
        // The capping attempt deletes the row.
        assert!(!record_challenge_attempt(&pool, challenge.id).await.unwrap());
        assert!(
            find_challenge(&pool, "hash-1", "web")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn challenge_consumes_once(pool: PgPool) {
        let user_id = seed_user(&pool).await;
        create_challenge(&pool, "hash-1", user_id, "web", None)
            .await
            .unwrap();
        let challenge = find_challenge(&pool, "hash-1", "web")
            .await
            .unwrap()
            .unwrap();
        // The first consume wins the single-use claim; a second (a replayed or
        // concurrent completion of the same assertion) loses it.
        assert!(consume_challenge(&pool, challenge.id).await.unwrap());
        assert!(!consume_challenge(&pool, challenge.id).await.unwrap());
        assert!(
            find_challenge(&pool, "hash-1", "web")
                .await
                .unwrap()
                .is_none()
        );
    }

    /// The claim and the counter bump commit together — a won
    /// claim always carries the credential update, a lost claim never touches
    /// the credential, and concurrent completions of one challenge admit
    /// exactly one winner.
    #[sqlx::test(migrations = "./migrations")]
    async fn consume_with_credential_update_is_atomic_and_single_winner(pool: PgPool) {
        let user_id = seed_user(&pool).await;
        let cred = crate::webauthn_credential::create(
            &pool,
            user_id,
            "key-1",
            "yubi",
            &serde_json::json!({ "cred": "initial" }),
            1,
        )
        .await
        .unwrap();

        // Winning the claim persists the bumped counter in the same commit.
        create_challenge(&pool, "hash-1", user_id, "web", None)
            .await
            .unwrap();
        let challenge = find_challenge(&pool, "hash-1", "web")
            .await
            .unwrap()
            .unwrap();
        let bumped = serde_json::json!({ "cred": "bumped" });
        assert!(
            consume_challenge_updating_credential(&pool, challenge.id, cred.id, &bumped, 5)
                .await
                .unwrap()
        );
        let keys = crate::webauthn_credential::list_by_user(&pool, user_id)
            .await
            .unwrap();
        assert_eq!(keys[0].sign_count, 5);

        // A lost claim (already-consumed challenge) must not touch the
        // credential: the transaction rolls back before the update.
        assert!(
            !consume_challenge_updating_credential(
                &pool,
                challenge.id,
                cred.id,
                &serde_json::json!({ "cred": "stale" }),
                99,
            )
            .await
            .unwrap()
        );
        let keys = crate::webauthn_credential::list_by_user(&pool, user_id)
            .await
            .unwrap();
        assert_eq!(keys[0].sign_count, 5, "lost claim must not bump");
        assert_eq!(keys[0].credential, bumped);

        // Concurrent completions of one challenge: exactly one winner, and the
        // counter lands on the winner's value exactly once.
        create_challenge(&pool, "hash-2", user_id, "web", None)
            .await
            .unwrap();
        let challenge = find_challenge(&pool, "hash-2", "web")
            .await
            .unwrap()
            .unwrap();
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let pool = pool.clone();
                let bumped = serde_json::json!({ "cred": "raced" });
                tokio::spawn(async move {
                    consume_challenge_updating_credential(&pool, challenge.id, cred.id, &bumped, 6)
                        .await
                        .unwrap()
                })
            })
            .collect();
        let mut winners = 0;
        for task in tasks {
            if task.await.unwrap() {
                winners += 1;
            }
        }
        assert_eq!(winners, 1, "exactly one concurrent completion may win");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn backup_codes_consume_once(pool: PgPool) {
        let user_id = seed_user(&pool).await;
        replace_backup_codes(&pool, user_id, &["a".to_owned(), "b".to_owned()])
            .await
            .unwrap();
        assert_eq!(backup_codes_remaining(&pool, user_id).await.unwrap(), 2);
        assert!(consume_backup_code(&pool, user_id, "a").await.unwrap());
        assert!(!consume_backup_code(&pool, user_id, "a").await.unwrap());
        assert_eq!(backup_codes_remaining(&pool, user_id).await.unwrap(), 1);

        // Regeneration replaces the set.
        replace_backup_codes(&pool, user_id, &["c".to_owned()])
            .await
            .unwrap();
        assert!(!consume_backup_code(&pool, user_id, "b").await.unwrap());
        assert!(consume_backup_code(&pool, user_id, "c").await.unwrap());
    }

    /// TOTP enrollment claims the timestep, enables the login
    /// requirement, and replaces the recovery codes as one transaction. A failed
    /// step leaves nothing half-applied — 2FA is never on without freshly issued
    /// codes, and a rolled-back attempt does not consume its timestep.
    #[sqlx::test(migrations = "./migrations")]
    async fn confirm_otp_enrollment_is_all_or_nothing(pool: PgPool) {
        let user_id = seed_user(&pool).await;
        let codes = vec!["c1".to_owned(), "c2".to_owned()];

        // No provisional secret → NoSecret; the timestep claim rolls back too, so
        // nothing changed.
        assert_eq!(
            confirm_otp_enrollment(&pool, user_id, 100, &codes)
                .await
                .unwrap(),
            OtpEnrollment::NoSecret
        );
        assert_eq!(backup_codes_remaining(&pool, user_id).await.unwrap(), 0);
        let u = crate::user::find_by_id(&pool, user_id)
            .await
            .unwrap()
            .unwrap();
        assert!(!u.otp_required_for_login);
        assert!(u.otp_consumed_timestep.is_none(), "timestep rolled back");

        // Provision a secret; the same timestep (still unconsumed) now enables 2FA
        // and stores the codes together.
        crate::user::set_otp_secret(&pool, user_id, "enc")
            .await
            .unwrap();
        assert_eq!(
            confirm_otp_enrollment(&pool, user_id, 100, &codes)
                .await
                .unwrap(),
            OtpEnrollment::Enabled
        );
        let u = crate::user::find_by_id(&pool, user_id)
            .await
            .unwrap()
            .unwrap();
        assert!(u.otp_required_for_login);
        assert_eq!(u.otp_consumed_timestep, Some(100));
        assert_eq!(backup_codes_remaining(&pool, user_id).await.unwrap(), 2);

        // Replaying the consumed timestep is refused and leaves the codes intact.
        assert_eq!(
            confirm_otp_enrollment(&pool, user_id, 100, &["x".to_owned()])
                .await
                .unwrap(),
            OtpEnrollment::CodeReused
        );
        assert_eq!(backup_codes_remaining(&pool, user_id).await.unwrap(), 2);
    }
}
