"""Session-owned, sealed migration-only schema for disposable E2E peers.

No application initialization runs in the template: every clone still creates
its own actor keys, settings cache, media and process through the real CLI.
Independent backup/restore databases continue to start from template0.
"""

import hashlib
import os
import secrets
import subprocess
import tempfile
import time
from contextlib import contextmanager

from . import candidate, config

_active = None


@contextmanager
def session():
    global _active
    if _active is not None:
        raise RuntimeError("E2E schema template session already active")
    template = SchemaTemplate()
    _active = template
    try:
        yield template
    finally:
        _active = None
        template.close()


def active_name():
    """No active session retains the original blank-database startup path."""
    return _active.for_clone() if _active is not None else None


class SchemaTemplate:
    def __init__(self):
        self.name = f"plamenu_e2e_schema_{secrets.token_hex(8)}"
        self.base_dsn = config.DB_DSN
        self.dsn = self.base_dsn.rsplit("/", 1)[0] + f"/{self.name}"
        self.owned = False
        self.identity = None
        self.oid = None
        self.binary_sha256 = None
        self.preparation_seconds = None
        self.log_path = None
        self.clones = 0

    @staticmethod
    def _identity():
        binary = candidate.binary_path()
        stat = binary.stat()
        binary_version = (
            stat.st_dev,
            stat.st_ino,
            stat.st_size,
            stat.st_mtime_ns,
            stat.st_ctime_ns,
            stat.st_mode,
        )
        files = sorted((config.PLAMENU_DIR / "crates/db/migrations").glob("*.sql"))
        if not files:
            raise RuntimeError("E2E migration source is missing")
        migrations = tuple(
            (path.name, hashlib.sha384(path.read_bytes()).hexdigest()) for path in files
        )
        return config.DB_DSN, str(binary), binary_version, migrations

    def for_clone(self):
        import psycopg

        if self.identity is None:
            self._prepare()
        if self._identity() != self.identity:
            raise RuntimeError(
                "E2E candidate, database or migration source changed during template session"
            )
        with psycopg.connect(self.base_dsn, autocommit=True) as conn:
            actual = conn.execute(
                "SELECT oid, datallowconn, datistemplate FROM pg_database WHERE datname = %s",
                (self.name,),
            ).fetchone()
        if actual != (self.oid, False, False):
            raise RuntimeError(
                "Owned E2E schema template is missing, replaced or unsealed"
            )
        self.clones += 1
        return self.name

    def _prepare(self):
        import psycopg
        from psycopg import sql

        began = time.monotonic()
        self.identity = self._identity()
        if self.identity[0] != self.base_dsn:
            raise RuntimeError("E2E database changed before template preparation")
        self.binary_sha256 = hashlib.sha256(
            candidate.binary_path().read_bytes()
        ).hexdigest()
        if self._identity() != self.identity:
            raise RuntimeError(
                "E2E candidate or migrations changed while binding template"
            )
        with psycopg.connect(self.base_dsn, autocommit=True) as conn:
            conn.execute(
                sql.SQL("CREATE DATABASE {} TEMPLATE template0").format(
                    sql.Identifier(self.name)
                )
            )
            self.owned = True
        logs = config.PLAMENU_DIR / "target/e2e/logs"
        logs.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(
            prefix="schema-", suffix=".log", dir=logs, delete=False
        ) as log:
            self.log_path = log.name
            result = subprocess.run(
                [
                    "cargo",
                    "sqlx",
                    "migrate",
                    "run",
                    "--source",
                    str(config.PLAMENU_DIR / "crates/db/migrations"),
                ],
                cwd=config.PLAMENU_DIR,
                env={**os.environ, "DATABASE_URL": self.dsn},
                stdout=log,
                stderr=subprocess.STDOUT,
                check=False,
                timeout=1800,
            )
        if result.returncode:
            raise RuntimeError(
                f"E2E schema migrations failed ({result.returncode}); log: {self.log_path}"
            )
        if self._identity() != self.identity:
            raise RuntimeError(
                "E2E candidate or migration source changed during template preparation"
            )
        expected = {
            int(name.split("_", 1)[0]): digest for name, digest in self.identity[3]
        }
        with psycopg.connect(self.dsn, autocommit=True) as conn:
            rows = conn.execute(
                "SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version"
            ).fetchall()
            if (
                any(not success for _, _, success in rows)
                or {version: bytes(checksum).hex() for version, checksum, _ in rows}
                != expected
            ):
                raise RuntimeError(
                    "E2E schema migration ledger differs from bound source"
                )
            for table in (
                "accounts",
                "users",
                "statuses",
                "actor_keys",
                "instance_actor_keys",
                "media_attachments",
            ):
                if conn.execute(
                    sql.SQL("SELECT count(*) FROM {}").format(sql.Identifier(table))
                ).fetchone() != (0,):
                    raise RuntimeError(
                        f"E2E schema template contains application data: {table}"
                    )
        with psycopg.connect(self.base_dsn, autocommit=True) as conn:
            conn.execute(
                sql.SQL("ALTER DATABASE {} ALLOW_CONNECTIONS false").format(
                    sql.Identifier(self.name)
                )
            )
            self.oid = conn.execute(
                "SELECT oid FROM pg_database WHERE datname = %s", (self.name,)
            ).fetchone()[0]
        self.preparation_seconds = time.monotonic() - began

    def close(self):
        if self.owned:
            import psycopg
            from psycopg import sql

            with psycopg.connect(self.base_dsn, autocommit=True) as conn:
                conn.execute(
                    sql.SQL("DROP DATABASE {} WITH (FORCE)").format(
                        sql.Identifier(self.name)
                    )
                )
            self.owned = False

    def report(self):
        return {
            "name": self.name,
            "candidate_sha256": self.binary_sha256,
            "preparation_seconds": self.preparation_seconds,
            "clone_requests": self.clones,
            "migration_files": dict(self.identity[3]) if self.identity else {},
            "log_path": self.log_path,
        }
