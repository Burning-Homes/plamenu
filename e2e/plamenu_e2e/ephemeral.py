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

An instance gets its own uniquely named database on the dev Postgres (removed
on teardown), its own media directory and its own port; Caddy
terminates TLS for the domain on the same local CA as the rest of the stack,
so the two instances federate over real HTTPS exactly as foreign peers do
(see e2e/README.md "One-time wiring").
"""

import os
import secrets
import socket
import subprocess
import tempfile
import threading
import time
import tomllib
from contextlib import ExitStack
from pathlib import Path

import psycopg
import requests
from psycopg import sql

from . import candidate, config, plamenu, schema_template, unique
from .api import Api
from .db import Db
from .lifecycle import Lease, owns_listener, stop_process

# The self-destruct peer. Module-level constants (rather than instance
# attributes) because the self-destruct test also drives raw HTTP against it.
DOMAIN = "doomed.local"
URL = f"https://{DOMAIN}"
BIND = "0.0.0.0:8421"
DB_NAME = "plamenu_doomed"

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
        db_name: str | None = None,
        conversation_containers: bool = False,
    ):
        self.domain = domain
        self.url = f"https://{domain}"
        self.bind = bind
        self.db_name = db_name or f"{DB_NAME}_{secrets.token_hex(8)}"
        self.dsn = config.DB_DSN.rsplit("/", 1)[0] + f"/{self.db_name}"
        self._conversation_containers = conversation_containers
        self._proc: subprocess.Popen | None = None
        self._dir: tempfile.TemporaryDirectory | None = None
        self._db: Db | None = None
        self._scalar_thread = threading.get_ident()
        self._scalar_connection: psycopg.Connection | None = None
        self._scalar_dsn = None
        self._lease = None
        self._database_owned = False
        self._log = None
        self._log_path = None

    @property
    def _config_path(self) -> Path:
        return Path(self._dir.name) / "plamenu.toml"

    def start(self, timeout: float = 120.0, *, suite_settings: bool = False) -> None:
        if self._dir is not None or self._database_owned or self._lease is not None:
            raise RuntimeError(
                "instance already started; stop it before starting again"
            )
        # Resolve/validate an explicit candidate before any fixture mutations;
        # ordinary development still builds the current default executable.
        candidate.prepare_binary()
        try:
            self._lease = Lease(f"e2e-listener:{self.bind.rsplit(':', 1)[1]}")
            self._assert_bind_available()
            self._start(timeout, suite_settings=suite_settings)
        except BaseException:
            self.stop()
            raise

    def _start(self, timeout, *, suite_settings=False):
        self.create_database(migrated=True)
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
        if suite_settings:
            # The real CLI performs the same migrations/key initialization as
            # serve, but starts no HTTP listener or background workers. Seed
            # only this new owned database before the server's first cache read.
            self.cli("role", "list")
            self.apply_suite_settings()
        self._launch(timeout, append_log=False)

    def create_database(self, *, migrated: bool = False) -> None:
        """Create and own a fresh database, also for independent restores."""
        if self._database_owned:
            raise RuntimeError("instance already owns a database")
        template = schema_template.active_name() if migrated else None
        # CREATE must fail if a caller supplied somebody else's database name.
        # Only a successfully created database belongs to this object.
        with psycopg.connect(config.DB_DSN, autocommit=True) as conn:
            # Some developer harnesses seed template1. A disposable instance
            # must not inherit unrelated accounts, migrations or private data.
            if template is None:
                conn.execute(
                    sql.SQL("CREATE DATABASE {} TEMPLATE template0").format(
                        sql.Identifier(self.db_name)
                    )
                )
            else:
                conn.execute(
                    sql.SQL("CREATE DATABASE {} TEMPLATE {}").format(
                        sql.Identifier(self.db_name), sql.Identifier(template)
                    )
                )
            self._database_owned = True

    def _launch(self, timeout: float, *, append_log: bool) -> None:
        try:
            self._launch_process(timeout, append_log=append_log)
        except BaseException:
            self.stop()
            raise

    def _launch_process(self, timeout: float, *, append_log: bool) -> None:
        # Recovery drills can prepare a restored DB/config without start().
        if self._lease is None:
            self._lease = Lease(f"e2e-listener:{self.bind.rsplit(':', 1)[1]}")
            self._assert_bind_available()
        log_path = Path(self._dir.name) / "server.log"
        self._log_path = log_path
        self._log = open(log_path, "a" if append_log else "w")  # noqa: SIM115 — process lifetime
        self._proc = subprocess.Popen(
            [str(binary_path()), "--config", str(self._config_path), "serve"],
            cwd=self._dir.name,
            stdout=self._log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            # Same verbosity the dev server runs at: when a cross-instance
            # assertion times out, this log is the only account of what the
            # peer did with the delivery.
            env={
                **os.environ,
                "RUST_LOG": "plamenu=debug,tower_http=info",
            },
        )
        print(f"   {self.domain} starting (pid {self._proc.pid}, log {log_path})")
        deadline = time.monotonic() + timeout
        # Readiness is a condition, not a calibrated observation window. Catch
        # a fast local start without an unconditional half-second delay; after
        # these bounded early observations retain the existing slow-start load.
        delays = iter((0.1,) * 10)
        while True:
            if self._proc.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError(
                    f"{self.domain} did not come up; log tail:\n{self.log_tail()}"
                )
            try:
                if self._ready():
                    return
            except (requests.RequestException, psycopg.Error, OSError):
                pass
            time.sleep(min(next(delays, 0.5), max(0.0, deadline - time.monotonic())))

    def _assert_bind_available(self) -> None:
        """Do not reset a database if a different server owns the fixture port."""
        address, port = self.bind.rsplit(":", 1)
        address = address.strip("[]")
        family = socket.AF_INET6 if ":" in address else socket.AF_INET
        try:
            with socket.socket(family, socket.SOCK_STREAM) as listener:
                listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                listener.bind((address, int(port)))
                listener.listen(1)
        except OSError as error:
            raise RuntimeError(
                f"{self.domain} fixture port {self.bind} is occupied; "
                "stop the existing owner before starting this disposable peer"
            ) from error

    def release_listener_after_exit(self) -> None:
        """Hand a quiesced fixture's port to an independent restore.

        Keep its database and media for backup assertions, but never release
        ownership while the source server or an inherited listener can run.
        """
        if self._proc is not None:
            if self._proc.poll() is None:
                raise RuntimeError("cannot hand off a running fixture listener")
            stop_process(self._proc)
        self._assert_bind_available()
        if self._lease is not None:
            self._lease.close()
            self._lease = None

    def _owns_listener(self) -> bool:
        """A foreign healthy service cannot establish this child's readiness."""
        # Fault drills move the child behind a proxy while keeping its public
        # URL unchanged. Verify the child's configured listener, not the proxy.
        bind = (
            tomllib.loads(self._config_path.read_text())["bind"]
            if self._dir is not None
            else self.bind
        )
        return owns_listener(self._proc, int(bind.rsplit(":", 1)[1]))

    def _ready(self) -> bool:
        if not self._owns_listener():
            return False
        with psycopg.connect(self.dsn, autocommit=True) as conn:
            initialized = conn.execute(
                "SELECT to_regclass('public.instance_settings') IS NOT NULL"
            ).fetchone()[0]
        if not initialized:
            return False
        healthy = requests.get(f"{self.url}/health", verify=False, timeout=5).ok
        return healthy and self._proc.poll() is None

    def restart_process_after_crash(self, timeout: float = 120.0) -> None:
        """Kill only the server process and relaunch against the same DB/media.

        Unlike ``start()``, this does not recreate the database or source
        directory. It is for explicit crash-recovery E2E checks on disposable
        instances, never the standing development server.
        """
        if self._proc is None or self._dir is None:
            raise RuntimeError("cannot restart an instance that was not started")
        self._proc.kill()
        stop_process(self._proc)
        self._proc = None
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
        # Polling on the owning thread need not fork a PostgreSQL backend for
        # every observation. Other threads retain independent short sessions.
        if threading.get_ident() != self._scalar_thread:
            with psycopg.connect(self.dsn, autocommit=True) as conn:
                row = conn.execute(sql, params).fetchone()
                return row[0] if row else None
        if (
            self._scalar_connection is None
            or self._scalar_connection.closed
            or self._scalar_dsn != self.dsn
        ):
            self._close_scalar_connection()
            self._scalar_connection = psycopg.connect(
                self.dsn, autocommit=True, prepare_threshold=None
            )
            self._scalar_dsn = self.dsn
        try:
            row = self._scalar_connection.execute(sql, params).fetchone()
            # Preserve the old per-call transaction/session boundary: no
            # settings, temporary objects or advisory locks survive a poll.
            # DISCARD also clears server prepared statements, so preparation
            # is disabled for this connection just as for the old one-shot use.
            self._scalar_connection.commit()
            self._scalar_connection.execute("DISCARD ALL")
        except BaseException:
            self._close_scalar_connection()
            raise
        return row[0] if row else None

    def _close_scalar_connection(self):
        connection, self._scalar_connection = self._scalar_connection, None
        self._scalar_dsn = None
        if connection is not None:
            connection.close()

    def apply_suite_settings(self) -> None:
        """Seed an owned, initialized database before serving any requests.

        Use start(suite_settings=True). Live cache-expiry tests still exercise
        the production TTL; fixture setup must never mutate a running cache.
        """
        if not self._database_owned or (
            self._proc is not None and self._proc.poll() is None
        ):
            raise RuntimeError(
                "Suite settings require an owned database and stopped server"
            )
        with psycopg.connect(self.dsn, autocommit=True) as conn:
            row = conn.execute(
                "UPDATE instance_settings SET rate_limiting_enabled = false,"
                " timeline_preview_federated = true, timeline_preview_local = true,"
                " timeline_preview_tag = true, public_search = true RETURNING true"
            ).fetchone()
            if row is None:
                raise RuntimeError("Fresh instance has no settings row")

    # ── lifecycle ─────────────────────────────────────────────────────

    def log_tail(self, lines: int = 40) -> str:
        if self._log_path is None:
            return "(never started)"
        try:
            return "\n".join(self._log_path.read_text().splitlines()[-lines:])
        except OSError as exc:
            return f"(no log: {exc})"

    def stop(self) -> None:
        # Every release is attempted even if another release fails. Clear
        # handles first so pytest finalizers may safely call stop() again.
        with ExitStack() as cleanup:
            if self._lease is not None:
                cleanup.callback(self._lease.close)
                self._lease = None
            if self._dir is not None:
                cleanup.callback(self._dir.cleanup)
                cleanup.callback(self._retain_log, Path(self._dir.name))
                self._dir = None
            if self._database_owned:
                cleanup.callback(self._drop_database)
                self._database_owned = False
            if self._log is not None:
                cleanup.callback(self._log.close)
                self._log = None
            if self._db is not None:
                cleanup.callback(self._db.close)
                self._db = None
            cleanup.callback(self._close_scalar_connection)
            if self._proc is not None:
                cleanup.callback(stop_process, self._proc)
                self._proc = None

    def _retain_log(self, directory):
        source = directory / "server.log"
        if source.exists():
            logs = directory.parent.parent / "logs"
            logs.mkdir(mode=0o700, exist_ok=True)
            destination = logs / f"{directory.name}.log"
            destination.write_bytes(source.read_bytes())
            destination.chmod(0o600)
            self._log_path = destination

    def _drop_database(self):
        with psycopg.connect(config.DB_DSN, autocommit=True) as conn:
            conn.execute(
                sql.SQL("DROP DATABASE IF EXISTS {} WITH (FORCE)").format(
                    sql.Identifier(self.db_name)
                )
            )


def peer() -> Instance:
    """The standing Plamenu-against-Plamenu peer (https://plamenu2.local).

    Conversation containers are on, matching the dev instance's config, so
    the FEP-171b path has a reader that actually consumes what it emits."""
    return Instance(
        domain=PEER_DOMAIN,
        bind=PEER_BIND,
        db_name=f"{PEER_DB_NAME}_{secrets.token_hex(8)}",
        conversation_containers=True,
    )
