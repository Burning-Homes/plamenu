"""Read-only queries against Plamenu's Postgres, for asserting on the rows
that federation must produce (follows, statuses, quotes, notifications)."""

import psycopg


class Db:
    def __init__(self, dsn: str):
        self._dsn = dsn
        self._conn: psycopg.Connection | None = None

    @property
    def conn(self) -> psycopg.Connection:
        if self._conn is None or self._conn.closed:
            self._conn = psycopg.connect(self._dsn, autocommit=True)
        return self._conn

    def close(self) -> None:
        if self._conn is not None and not self._conn.closed:
            self._conn.close()

    def _value(self, sql: str, *params):
        row = self.conn.execute(sql, params).fetchone()
        return row[0] if row else None

    # ── domain queries ────────────────────────────────────────────────

    def follower_count(self, username: str) -> int:
        """Rows in `follows` targeting a local user."""
        return self._value(
            "SELECT count(*) FROM follows f"
            " JOIN accounts t ON t.id = f.target_account_id"
            " WHERE t.username = %s",
            username,
        )

    def delete_remote_follower(
        self, target_username: str, follower_username: str, follower_domain: str
    ) -> None:
        """Create follower drift by deleting one remote follower row locally."""
        self.conn.execute(
            "DELETE FROM follows f"
            " USING accounts follower, accounts target"
            " WHERE f.account_id = follower.id"
            " AND f.target_account_id = target.id"
            " AND target.username = %s AND target.domain IS NULL"
            " AND follower.username = %s AND follower.domain = %s",
            (target_username, follower_username, follower_domain),
        )

    def forget_remote_account(self, uri: str) -> None:
        """Delete a stored remote account (cascading its rows) and clear its
        host's signature-pref, forcing the next resolve to re-fetch the actor
        fresh — used to deterministically re-trigger FEP-844e capability
        learning regardless of what a persisted dev DB already cached."""
        from urllib.parse import urlparse

        host = urlparse(uri).hostname
        self.conn.execute("DELETE FROM host_signature_prefs WHERE host = %s", (host,))
        self.conn.execute("DELETE FROM accounts WHERE uri = %s", (uri,))

    def forget_host(self, host: str) -> None:
        """Erase every trace of a peer host: its stored accounts (and, by
        cascade, their statuses, follows and notifications), the queued
        deliveries aimed at it, the finite fetch-failure budget, the delivery
        breaker and the learned signature preference.

        The Plamenu-against-Plamenu peer is a *fresh install* on every test
        session — new database, new instance-actor key, same domain. Without
        this, the previous session's abandoned fetch budget (recorded while
        the peer was being torn down) would suppress the first fetch of the
        new one, and its cached actor keys would no longer match. A real
        operator reinstalling a server is the same situation; that peers
        recover from it on their own schedule, rather than instantly, is the
        deliberate behaviour the breaker exists for."""
        self.conn.execute("DELETE FROM accounts WHERE domain = %s", (host,))
        self.conn.execute(
            "DELETE FROM delivery_jobs WHERE inbox_url LIKE %s", (f"https://{host}/%",)
        )
        self.conn.execute(
            "DELETE FROM remote_fetch_failures WHERE failure_key LIKE %s",
            (f"%{host}%",),
        )
        self.conn.execute("DELETE FROM host_reachability WHERE host = %s", (host,))
        self.conn.execute("DELETE FROM host_signature_prefs WHERE host = %s", (host,))

    def remote_fetch_failures_for(self, host: str) -> int:
        """Finite fetch-budget rows involving a peer host."""
        return self._value(
            "SELECT count(*) FROM remote_fetch_failures WHERE failure_key LIKE %s",
            f"%{host}%",
        )

    def outbound_follow_pending(self, username: str) -> bool | None:
        """`pending` of the local user's outbound follow (None if no row)."""
        return self._value(
            "SELECT f.pending FROM follows f"
            " JOIN accounts l ON l.id = f.account_id"
            " WHERE l.username = %s",
            username,
        )

    def remote_suspension(self, username: str, domain: str) -> str | None:
        """The suspension state of a stored remote account: `None` while not
        suspended, otherwise the recorded `suspension_origin`."""
        return self._value(
            "SELECT CASE WHEN suspended_at IS NULL THEN NULL"
            "            ELSE coalesce(suspension_origin, '?') END"
            " FROM accounts WHERE username = %s AND domain = %s",
            username,
            domain,
        )

    def attribution_domains(
        self, username: str, domain: str | None
    ) -> list[str] | None:
        """The stored `attribution_domains` of an account (None if unknown)."""
        if domain is None:
            return self._value(
                "SELECT attribution_domains FROM accounts"
                " WHERE username = %s AND domain IS NULL",
                username,
            )
        return self._value(
            "SELECT attribution_domains FROM accounts"
            " WHERE username = %s AND domain = %s",
            username,
            domain,
        )

    def pending_deliveries_to(self, domain: str) -> int:
        """Outbound delivery jobs still queued for inboxes on `domain`. A
        skipped delivery drops its row, so this draining to 0 proves the
        worker has processed (and, under a suspension, discarded) them."""
        return self._value(
            "SELECT count(*) FROM delivery_jobs WHERE inbox_url LIKE %s",
            f"https://{domain}/%",
        )

    def inbound_block_count(self, username: str) -> int:
        """Rows in `blocks` where a remote account blocks the local user."""
        return self._value(
            "SELECT count(*) FROM blocks b"
            " JOIN accounts t ON t.id = b.target_account_id"
            " JOIN accounts s ON s.id = b.account_id"
            " WHERE t.username = %s AND t.domain IS NULL AND s.domain IS NOT NULL",
            username,
        )

    def inbound_report_count(self, username: str, marker: str) -> int:
        """Rows in `reports` where a remote account reported the local user,
        whose comment carries `marker` (the per-test unique token)."""
        return self._value(
            "SELECT count(*) FROM reports r"
            " JOIN accounts t ON t.id = r.target_account_id"
            " JOIN accounts s ON s.id = r.account_id"
            " WHERE t.username = %s AND t.domain IS NULL AND s.domain IS NOT NULL"
            " AND r.comment LIKE '%%' || %s || '%%'",
            username,
            marker,
        )

    def enable_statuses_cleanup(self, username: str, min_status_age_secs: int) -> None:
        """Enable auto-deletion for a local user with an artificially small
        min_status_age. The settings form only offers ≥ 1 week, but the sweep
        honors whatever the row says — which lets the e2e watch a freshly
        federated post get swept within one 60s sweep cycle."""
        self.conn.execute(
            "INSERT INTO account_statuses_cleanup_policies (account_id, min_status_age)"
            " SELECT id, %s FROM accounts WHERE username = %s AND domain IS NULL"
            " ON CONFLICT (account_id) DO UPDATE"
            " SET min_status_age = EXCLUDED.min_status_age,"
            "     enabled = TRUE, last_inspected_id = NULL",
            (min_status_age_secs, username),
        )

    def status_id_containing(self, marker: str) -> int | None:
        return self._value(
            "SELECT id FROM statuses WHERE content LIKE '%%' || %s || '%%'", marker
        )

    def status_parent_id(self, status_id: int) -> int | None:
        """`in_reply_to_id` of a status (None when not a reply)."""
        return self._value(
            "SELECT in_reply_to_id FROM statuses WHERE id = %s", status_id
        )

    def local_actor_uri(self, username: str) -> str | None:
        """Canonical ActivityPub actor ID for a local account."""
        return self._value(
            "SELECT uri FROM accounts WHERE username = %s AND domain IS NULL",
            username,
        )

    def status_visibility(self, status_id: int) -> str | None:
        return self._value("SELECT visibility FROM statuses WHERE id = %s", status_id)

    def remote_moved_to(self, username: str, domain: str) -> str | None:
        """The recorded `movedTo` redirect of a stored remote account (None if
        not moved or unknown)."""
        return self._value(
            "SELECT moved_to_uri FROM accounts WHERE username = %s AND domain = %s",
            username,
            domain,
        )

    def remote_account_exists(self, username: str, domain: str) -> bool:
        """Whether a remote account row is still present (False after a
        federated Delete(Actor) removes it)."""
        return bool(
            self._value(
                "SELECT 1 FROM accounts WHERE username = %s AND domain = %s",
                username,
                domain,
            )
        )

    def outbound_follow_exists(
        self, follower_username: str, target_username: str, target_domain: str
    ) -> bool:
        """Whether a local user follows a specific remote target — used to
        verify Move re-pointing and Delete(Actor) severance."""
        return bool(
            self._value(
                "SELECT 1 FROM follows f"
                " JOIN accounts follower ON follower.id = f.account_id"
                " JOIN accounts target ON target.id = f.target_account_id"
                " WHERE follower.username = %s AND follower.domain IS NULL"
                " AND target.username = %s AND target.domain = %s",
                follower_username,
                target_username,
                target_domain,
            )
        )

    def remote_display_name(self, username: str, domain: str) -> str | None:
        return self._value(
            "SELECT display_name FROM accounts WHERE username = %s AND domain = %s",
            username,
            domain,
        )

    def remote_avatar_url(self, username: str, domain: str) -> str | None:
        return self._value(
            "SELECT avatar_remote_url FROM accounts WHERE username = %s AND domain = %s",
            username,
            domain,
        )

    def status_edited_at(self, status_id: int):
        return self._value("SELECT edited_at FROM statuses WHERE id = %s", status_id)

    def quote_state_for_quoted(self, username: str) -> str | None:
        """State of the quote whose quoted post belongs to a local user."""
        return self._value(
            "SELECT q.state FROM quotes q"
            " JOIN accounts quoted ON quoted.id = q.quoted_account_id"
            " WHERE quoted.username = %s",
            username,
        )

    def quote_for_status(self, status_id: int) -> tuple[str, str | None] | None:
        """(state, approval_uri) of the quote attached to a local status."""
        return self.conn.execute(
            "SELECT q.state, q.approval_uri FROM quotes q WHERE q.status_id = %s",
            (status_id,),
        ).fetchone()

    def status_cw(self, status_id: int) -> tuple[str, bool, str | None] | None:
        """(spoiler_text, sensitive, language) of a status row."""
        return self.conn.execute(
            "SELECT spoiler_text, sensitive, language FROM statuses WHERE id = %s",
            (status_id,),
        ).fetchone()

    def reaction_rows(self, status_id: int) -> list[tuple]:
        """(name, custom_emoji_url) of a status' stored reactions."""
        return self.conn.execute(
            "SELECT name, custom_emoji_url FROM status_reactions"
            " WHERE status_id = %s ORDER BY id",
            (status_id,),
        ).fetchall()

    def reblog_count(self, status_id: int) -> int:
        """Boost rows pointing at a status (what inbound Announce creates)."""
        return self._value(
            "SELECT count(*) FROM statuses WHERE reblog_of_id = %s", status_id
        )

    def favourite_count(self, status_id: int) -> int:
        return self._value(
            "SELECT count(*) FROM favourites WHERE status_id = %s", status_id
        )

    def notification_count(self, username: str, kind: str) -> int:
        return self._value(
            "SELECT count(*) FROM notifications n"
            " JOIN accounts a ON a.id = n.account_id"
            " WHERE a.username = %s AND n.kind = %s",
            username,
            kind,
        )

    def poll_for_status(self, status_id: int) -> tuple[int, list, list] | None:
        """(poll id, options, cached_tallies) of a status' poll, if any."""
        return self.conn.execute(
            "SELECT id, options, cached_tallies FROM polls WHERE status_id = %s",
            (status_id,),
        ).fetchone()

    def emoji_image_url(self, shortcode: str, domain: str | None) -> str | None:
        """The stored image URL of a custom emoji (`domain=None` = local)."""
        return self._value(
            "SELECT image_remote_url FROM custom_emojis"
            " WHERE shortcode = %s AND domain IS NOT DISTINCT FROM %s",
            shortcode,
            domain,
        )

    def poll_tallies(self, poll_id: int) -> list | None:
        return self._value("SELECT cached_tallies FROM polls WHERE id = %s", poll_id)

    def media_for_status(self, status_id: int) -> list[tuple]:
        """(content_type, description, blurhash, focus_x, focus_y, width,
        height) of a status' attachments, in id order."""
        return self.conn.execute(
            "SELECT content_type, description, blurhash, focus_x, focus_y,"
            " width, height FROM media_attachments"
            " WHERE status_id = %s ORDER BY id",
            (status_id,),
        ).fetchall()

    def rfc9421_pref(self, host: str) -> bool | None:
        """The remembered double-knock verdict for a host: True = the
        host accepted an RFC 9421 delivery, False = it refused and the
        draft-cavage retry landed, None = never delivered with the knock."""
        return self._value(
            "SELECT rfc9421 FROM host_signature_prefs WHERE host = %s", host
        )

    def ed25519_public_key(self, username: str, domain: str | None) -> str | None:
        """The stored FEP-521a Multikey of an account (`domain=None` = local)."""
        return self._value(
            "SELECT k.public_key FROM actor_keys k"
            " JOIN accounts a ON a.id = k.account_id"
            " WHERE a.username = %s AND a.domain IS NOT DISTINCT FROM %s"
            " AND k.owner_kind = 'account' AND k.algorithm = 'ed25519'"
            " AND k.revoked_at IS NULL AND k.retired_at IS NULL"
            " AND (k.expires_at IS NULL OR k.expires_at > now())"
            " ORDER BY k.activated_at DESC, k.id DESC LIMIT 1",
            username,
            domain,
        )
