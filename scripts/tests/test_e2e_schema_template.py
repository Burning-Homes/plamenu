"""Template boundaries fail before cloning when provenance or preparation fails."""

import hashlib
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "e2e"))
from plamenu_e2e import candidate, config, schema_template


class SchemaTemplateTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.binary = self.root / "candidate"
        self.binary.write_bytes(b"candidate-one")
        self.migrations = self.root / "crates/db/migrations"
        self.migrations.mkdir(parents=True)
        self.source = self.migrations / "0001_initial.sql"
        self.source.write_text("SELECT 1;\n")
        self.addCleanup(patch.stopall)
        patch.object(config, "PLAMENU_DIR", self.root).start()
        patch.object(config, "DB_DSN", "postgres://owned/test").start()
        patch.object(candidate, "binary_path", return_value=self.binary).start()
        self.db = types.ModuleType("psycopg")
        self.db.connect = MagicMock()
        self.db.sql = MagicMock()
        patch.dict(sys.modules, {"psycopg": self.db}).start()
        self.conn = self.db.connect.return_value.__enter__.return_value

    def prepared(self):
        template = schema_template.SchemaTemplate()
        template.identity = template._identity()
        template.oid = 42
        return template

    def test_session_is_lazy_rejects_nesting_and_releases_after_interrupt(self):
        with patch.object(schema_template.SchemaTemplate, "close") as close:
            with self.assertRaises(KeyboardInterrupt), schema_template.session():
                self.db.connect.assert_not_called()
                with (
                    self.assertRaisesRegex(RuntimeError, "already active"),
                    schema_template.session(),
                ):
                    self.fail("nested session accepted")
                raise KeyboardInterrupt
            close.assert_called_once()
        self.assertIsNone(schema_template.active_name())

    def test_source_candidate_and_database_changes_reject_before_any_connection(self):
        for change in ("binary", "source", "missing", "database", "path"):
            with self.subTest(change=change):
                self.binary.write_bytes(b"candidate-one")
                self.source.write_text("SELECT 1;\n")
                config.DB_DSN = "postgres://owned/test"
                template = self.prepared()
                if change == "binary":
                    self.binary.write_bytes(b"candidate-two")
                elif change == "source":
                    self.source.write_text("SELECT 2;\n")
                elif change == "missing":
                    self.source.unlink()
                elif change == "database":
                    config.DB_DSN = "postgres://foreign/test"
                else:
                    config.PLAMENU_DIR = self.root / "missing"
                with self.assertRaises(RuntimeError):
                    template.for_clone()
                config.PLAMENU_DIR = self.root
                self.db.connect.assert_not_called()

    def test_only_the_original_sealed_database_can_be_cloned(self):
        template = self.prepared()
        for row in (None, (43, False, False), (42, True, False), (42, False, True)):
            with self.subTest(row=row):
                self.conn.execute.return_value.fetchone.return_value = row
                with self.assertRaisesRegex(
                    RuntimeError, "missing, replaced or unsealed"
                ):
                    template.for_clone()
        self.conn.execute.return_value.fetchone.return_value = (42, False, False)
        self.assertEqual(template.for_clone(), template.name)
        self.assertEqual(template.clones, 1)

    def test_creation_collision_never_drops_an_unowned_database(self):
        self.conn.execute.side_effect = RuntimeError("already exists")
        template = schema_template.SchemaTemplate()
        with self.assertRaisesRegex(RuntimeError, "already exists"):
            template.for_clone()
        template.close()
        self.assertFalse(template.owned)
        self.assertEqual(self.db.connect.call_count, 1)
        self.assertEqual(self.db.sql.SQL.call_count, 1)

    def test_database_change_before_lazy_preparation_cannot_touch_the_old_origin(self):
        template = schema_template.SchemaTemplate()
        config.DB_DSN = "postgres://other/test"
        with self.assertRaisesRegex(RuntimeError, "database changed before"):
            template.for_clone()
        template.close()
        self.db.connect.assert_not_called()

    def test_failed_missing_or_timed_out_migration_command_cleans_owned_template(self):
        for outcome in (
            types.SimpleNamespace(returncode=1),
            FileNotFoundError("cargo"),
            subprocess.TimeoutExpired("cargo sqlx", 1800),
        ):
            with self.subTest(outcome=outcome):
                self.db.sql.SQL.reset_mock()
                with patch.object(schema_template.subprocess, "run") as run:
                    if isinstance(outcome, Exception):
                        run.side_effect = outcome
                    else:
                        run.return_value = outcome
                    with (
                        self.assertRaises(
                            (RuntimeError, FileNotFoundError, subprocess.TimeoutExpired)
                        ),
                        schema_template.session() as template,
                    ):
                        schema_template.active_name()
                    self.assertFalse(template.owned)
                    statements = [
                        call.args[0] for call in self.db.sql.SQL.call_args_list
                    ]
                    self.assertEqual(
                        statements,
                        [
                            "CREATE DATABASE {} TEMPLATE template0",
                            "DROP DATABASE {} WITH (FORCE)",
                        ],
                    )
                self.assertIsNone(schema_template.active_name())

    def test_bad_ledger_or_application_residue_cannot_be_sealed(self):
        for corruption in ("checksum", "failed", "residue"):
            with self.subTest(corruption=corruption):
                checksum = hashlib.sha384(self.source.read_bytes()).digest()
                self.conn.execute.return_value.fetchall.return_value = [
                    (
                        1,
                        b"wrong" if corruption == "checksum" else checksum,
                        corruption != "failed",
                    )
                ]
                self.conn.execute.return_value.fetchone.return_value = (1,)
                self.db.sql.SQL.reset_mock()
                with (
                    patch.object(
                        schema_template.subprocess,
                        "run",
                        return_value=types.SimpleNamespace(returncode=0),
                    ),
                    self.assertRaisesRegex(
                        RuntimeError, "ledger differs|contains application data"
                    ),
                    schema_template.session() as template,
                ):
                    schema_template.active_name()
                self.assertFalse(template.owned)
                self.assertFalse(
                    any(
                        "ALTER DATABASE" in call.args[0]
                        for call in self.db.sql.SQL.call_args_list
                    )
                )
