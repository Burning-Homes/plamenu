"""Second Plamenu instances the suite starts and stops itself.

Two of them, both driven by the same `Instance` class:

* **https://doomed.local** — for tests whose side effects would wreck the
  standing dev instance; today only the server self-destruct test, which
  suspends every local account and broadcasts their deletion to every peer.
* **https://plamenu2.local** — the peer for Plamenu-against-Plamenu
  federation. Every other peer in the fleet is foreign software, so nothing
  else exercises this server's own emission against its own ingest: the
  dialects it speaks (quotes, reactions, groups, events, long-form,
  conversation containers) only ever met a reader that implements a subset
  of them.

An instance gets its own throwaway database on the dev Postgres (dropped and
recreated on every start), its own media directory and its own port; Caddy
terminates TLS for the domain on the same local CA as the rest of the stack,
so the two instances federate over real HTTPS exactly as foreign peers do
(see e2e/README.md "One-time wiring").
"""

import os
import secrets
import subprocess
import tempfile
import time
from pathlib import Path

import psycopg
import requests
from psycopg import sql

from . import candidate, config, plamenu, unique
from .api import Api
from .db import Db

# The self-destruct peer. Module-level constants (rather than instance
# attributes) because the self-destruct test also drives raw HTTP against it.
DOMAIN = "doomed.local"
URL = f"https://{DOMAIN}"
BIND = "0.0.0.0:8421"
DB_NAME = "plamenu_doomed"
DSN = config.DB_DSN.rsplit("/", 1)[0] + f"/{DB_NAME}"

# The Plamenu-against-Plamenu peer.
PEER_DOMAIN = config.PLAMENU2_DOMAIN
PEER_URL = config.PLAMENU2_URL
PEER_BIND = "0.0.0.0:8422"
PEER_DB_NAME = "plamenu_peer"


def binary_path() -> Path:
    """Current candidate binding, also shared by frozen crash-drill consumers."""
    return candidate.binary_path()


class Instance:
    """A `plamenu serve` process on its own domain, plus its CLI and database."""

    def __init__(
        self,
        *,
        domain: str = DOMAIN,
        bind: str = BIND,
        db_name: str = DB_NAME,
        conversation_containers: bool = False,
    ):
        self.domain = domain
        self.url = f"https://{domain}"
        self.bind = bind
        self.db_name = db_name
        self.dsn = config.DB_DSN.rsplit("/", 1)[0] + f"/{db_name}"
        self._conversation_containers = conversation_containers
        self._proc: subprocess.Popen | None = None
        self._dir: tempfile.TemporaryDirectory | None = None
        self._db: Db | None = None

    @property
    def _config_path(self) -> Path:
        return Path(self._dir.name) / "plamenu.toml"

    def start(self, timeout: float = 120.0) -> None:
        # Resolve/validate an explicit candidate before any fixture mutations;
        # ordinary development still builds the current default executable.
        candidate.prepare_binary()
        # A fresh database every start: a previous run left accounts, follows
        # and (for doomed) a half-destroyed instance behind on purpose.
        with psycopg.connect(config.DB_DSN, autocommit=True) as conn:
            conn.execute(
                sql.SQL("DROP DATABASE IF EXISTS {} WITH (FORCE)").format(
                    sql.Identifier(self.db_name)
                )
            )
            # Some developer harnesses seed template1. A disposable instance
            # must not inherit unrelated accounts, migrations or private data.
            conn.execute(
                sql.SQL("CREATE DATABASE {} TEMPLATE template0").format(
                    sql.Identifier(self.db_name)
                )
            )
        # Use the real media filesystem, not /tmp (often a small RAM-backed
        # mount). Maximum-size video tests must exercise disk admission rather
        # than silently queue forever behind a tmpfs free-space fence.
        instances = config.PLAMENU_DIR / "target/e2e/instances"
        instances.mkdir(parents=True, exist_ok=True)
        self._dir = tempfile.TemporaryDirectory(
            prefix=f"plamenu-{self.db_name}-", dir=instances
        )
        self._config_path.write_text(
            f'domain = "{self.domain}"\n'
            f'database_url = "{self.dsn}"\n'
            f'bind = "{self.bind}"\n'
            'live_ingest_bind = "127.0.0.1:0"\n'
            f'encryption_secret = "{secrets.token_hex(32)}"\n'
            "allow_private_fetch = true\n"
            + (
                "conversation_containers = true\n"
                if self._conversation_containers
                else ""
            )
            + f'media_dir = "{Path(self._dir.name) / "media"}"\n'
        )
        self._launch(timeout, append_log=False)

    def _launch(self, timeout: float, *, append_log: bool) -> None:
        log_path = Path(self._dir.name) / "server.log"
        self._log = open(log_path, "a" if append_log else "w")  # noqa: SIM115 — process lifetime
        self._proc = subprocess.Popen(
            [str(binary_path()), "--config", str(self._config_path), "serve"],
            cwd=self._dir.name,
            stdout=self._log,
            stderr=subprocess.STDOUT,
            # Same verbosity the dev server runs at: when a cross-instance
            # assertion times out, this log is the only account of what the
            # peer did with the delivery.
            env={
                "RUST_LOG": os.environ.get("RUST_LOG", "plamenu=debug,tower_http=info"),
                **os.environ,
            },
        )
        print(f"   {self.domain} starting (pid {self._proc.pid}, log {log_path})")
        deadline = time.monotonic() + timeout
        while True:
            try:
                if requests.get(f"{self.url}/health", verify=False, timeout=5).ok:
                    return
            except requests.RequestException:
                pass
            if self._proc.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError(
                    f"{self.domain} did not come up; log tail:\n{self.log_tail()}"
                )
            time.sleep(0.5)

    def restart_process_after_crash(self, timeout: float = 120.0) -> None:
        """Kill only the server process and relaunch against the same DB/media.

        Unlike ``start()``, this does not recreate the database or source
        directory. It is for explicit crash-recovery E2E checks on disposable
        instances, never the standing development server.
        """
        if self._proc is None or self._dir is None:
            raise RuntimeError("cannot restart an instance that was not started")
        self._proc.kill()
        self._proc.wait(timeout=15)
        self._log.close()
        self._launch(timeout, append_log=True)

    def cli(self, *args: str, input_text: str | None = None) -> str:
        """Run the plamenu CLI against this instance's database."""
        proc = subprocess.run(
            [str(binary_path()), "--config", str(self._config_path), *args],
            input=input_text,
            capture_output=True,
            text=True,
            timeout=300,
            check=False,
        )
        if proc.returncode != 0:
            raise RuntimeError(
                f"{self.domain} cli failed (exit {proc.returncode}): "
                f"plamenu {' '.join(args)}\n{proc.stderr.strip()}"
            )
        return proc.stdout

    # ── accounts ──────────────────────────────────────────────────────

    def account(self, prefix: str = "peer", display_name: str = "E2E Peer"):
        """Create a fresh local account on this instance and return its
        credentials (a `plamenu.User` carrying this instance's domain)."""
        user = plamenu.User(username=unique(prefix), domain=self.domain)
        self.cli(
            "account",
            "add",
            user.username,
            "--display-name",
            display_name,
            "--email",
            user.email,
            "--password",
            user.password,
        )
        return user

    def login(self, user, scopes: str = "read write") -> Api:
        """Client-API session for one of this instance's accounts."""
        return plamenu.login(user, scopes=scopes, base_url=self.url)

    def user_api(self, prefix: str = "peer", **kwargs) -> tuple[plamenu.User, Api]:
        """A fresh account plus its client-API session."""
        user = self.account(prefix, **kwargs)
        return user, self.login(user)

    def admin(self, prefix: str = "peeradmin") -> tuple[plamenu.User, Api]:
        """A fresh `Owner` account on this instance with an admin-scoped
        client-API session — the peer-side twin of `plamenu.admin()`."""
        user = self.account(prefix)
        self.cli("account", "set-role", user.username, "Owner")
        return user, self.login(user, scopes="read write admin:read admin:write")

    @property
    def commands(self) -> plamenu.Cli:
        """The same CLI wrapper the suite uses on the dev instance (groups,
        posting, aliases, migration), bound to this instance."""
        return plamenu.Cli(run=self.cli)

    # ── database ──────────────────────────────────────────────────────

    @property
    def db(self) -> Db:
        """The same read-only query helpers the suite uses on the dev
        instance, pointed at this instance's database."""
        if self._db is None:
            self._db = Db(self.dsn)
        return self._db

    def db_value(self, sql: str, *params):
        """One scalar from this instance's database."""
        with psycopg.connect(self.dsn, autocommit=True) as conn:
            row = conn.execute(sql, params).fetchone()
            return row[0] if row else None

    def apply_suite_settings(self) -> None:
        """Pin the instance settings the suite depends on, exactly as
        `conftest._suite_instance_settings` does for the dev instance: rate
        limiting off (the whole suite drives the API from one client IP) and
        the open discovery posture the tests assert against. Waits out the
        settings cache TTL so the values are live before the first request."""
        with psycopg.connect(self.dsn, autocommit=True) as conn:
            conn.execute(
                "UPDATE instance_settings SET rate_limiting_enabled = false,"
                " timeline_preview_federated = true, timeline_preview_local = true,"
                " timeline_preview_tag = true, public_search = true"
            )
        time.sleep(6)

    # ── lifecycle ─────────────────────────────────────────────────────

    def log_tail(self, lines: int = 40) -> str:
        if self._dir is None:
            return "(never started)"
        path = Path(self._dir.name) / "server.log"
        try:
            return "\n".join(path.read_text().splitlines()[-lines:])
        except OSError as exc:
            return f"(no log: {exc})"

    def stop(self) -> None:
        if self._db is not None:
            self._db.close()
            self._db = None
        if self._proc is not None and self._proc.poll() is None:
            self._proc.terminate()
            try:
                self._proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self._proc.kill()
        if getattr(self, "_log", None) is not None:
            self._log.close()
        if self._dir is not None:
            self._dir.cleanup()
        # The database is left behind for post-mortem queries; the next
        # start() recreates it.


def peer() -> Instance:
    """The standing Plamenu-against-Plamenu peer (https://plamenu2.local).

    Conversation containers are on, matching the dev instance's config, so
    the FEP-171b path has a reader that actually consumes what it emits."""
    return Instance(
        domain=PEER_DOMAIN,
        bind=PEER_BIND,
        db_name=PEER_DB_NAME,
        conversation_containers=True,
    )
