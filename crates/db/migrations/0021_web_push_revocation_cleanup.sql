-- Web Push subscription cleanup on token revocation (QC audit #66).
--
-- A Web Push subscription is keyed to exactly one OAuth access token
-- (web_push_subscriptions.access_token_id, UNIQUE). Deleting a token cascades
-- its subscription away, but revocation is a *soft* delete — oauth_tokens.
-- revoked_at is stamped while the row stays — so a revoked session's
-- subscription, and therefore its share of every future notification's
-- fan-out, outlived the credential. A user mints a fresh browser/OAuth token
-- on every login or grant, and those tokens never expire server-side, so
-- without cleanup one account's accumulated revoked sessions kept multiplying
-- every notification's queue fan-out and outbound encrypted POSTs without
-- bound.
--
-- This trigger makes revocation delete the subscription at the moment ANY code
-- path sets revoked_at, whichever revocation entry point ran: logout,
-- signed-in password change, per-app or per-session revoke, revoke-all, or
-- roster/eviction supersession. It fires only on the NULL -> timestamp
-- transition, so a repeated revoke is a no-op. The subscription's pending
-- push_delivery_jobs and web_push_alerts cascade away with it (both are
-- ON DELETE CASCADE), so in-flight fan-out already queued for a
-- just-revoked session is cancelled too, and the freed slot no longer counts
-- against the per-user subscription cap.

CREATE FUNCTION public.web_push_drop_on_revoke() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    DELETE FROM web_push_subscriptions WHERE access_token_id = NEW.id;
    RETURN NULL;
END $$;

CREATE TRIGGER web_push_drop_on_revoke
    AFTER UPDATE OF revoked_at ON oauth_tokens
    FOR EACH ROW
    WHEN (OLD.revoked_at IS NULL AND NEW.revoked_at IS NOT NULL)
    EXECUTE FUNCTION public.web_push_drop_on_revoke();
