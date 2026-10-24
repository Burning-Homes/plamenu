"""SQLx checks tolerate overridden inference, never query/type changes or missing output."""

import contextlib
import copy
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from plamenu_release import checks
from plamenu_release.core import ReleaseError
from plamenu_release.sqlx import compare, effective_metadata


class TestDatabaseTemplateTests(unittest.TestCase):
    def test_owned_template_is_prepared_before_unfiltered_serial_tests(self):
        runner = Mock()
        url = "postgres://fixture@127.0.0.1/owned"
        with (
            patch.object(checks, "database", return_value=contextlib.nullcontext(url)),
            patch("plamenu_release.sqlx.check") as metadata,
        ):
            checks.application(runner, Path("evidence"))
        commands = runner.run.call_args_list
        self.assertEqual(len(commands), 3)
        self.assertEqual(commands[0].kwargs["env"]["DATABASE_URL"], url)
        self.assertEqual(
            commands[1].kwargs["env"]["DATABASE_URL"],
            url.rsplit("/", 1)[0] + "/template1",
        )
        self.assertEqual(
            commands[2].args,
            (
                "cargo",
                "nextest",
                "run",
                "--no-fail-fast",
                "--locked",
                "--workspace",
                "--test-threads",
                "1",
            ),
        )
        metadata.assert_called_once_with(runner, Path("evidence"), url)

    def test_template_failure_cannot_fall_through_to_a_successful_gate(self):
        runner = Mock()
        runner.run.side_effect = [None, ReleaseError("template failed")]
        with (
            patch.object(
                checks,
                "database",
                return_value=contextlib.nullcontext("postgres://fixture/owned"),
            ),
            patch("plamenu_release.sqlx.check") as metadata,
            self.assertRaisesRegex(ReleaseError, "template failed"),
        ):
            checks.application(runner, Path("evidence"))
        self.assertEqual(runner.run.call_count, 2)
        metadata.assert_not_called()


class SqlxMetadataTests(unittest.TestCase):
    def record(self):
        return {
            "db_name": "PostgreSQL",
            "query": 'SELECT a AS "a!", b AS "b?", c',
            "hash": "fixture",
            "describe": {
                "columns": [
                    {"name": "a!", "type_info": "Int8"},
                    {"name": "b?", "type_info": "Int8"},
                    {"name": "c", "type_info": "Int8"},
                ],
                "parameters": {"Left": ["Int8"]},
                "nullable": [False, True, False],
            },
        }

    def test_only_explicit_nullability_is_normalized(self):
        expected = self.record()
        original = copy.deepcopy(expected)
        different_plan = copy.deepcopy(expected)
        different_plan["describe"]["nullable"] = [True, False, False]
        self.assertEqual(
            effective_metadata(expected), effective_metadata(different_plan)
        )
        self.assertEqual(expected, original)
        different_plan["describe"]["nullable"][2] = True
        self.assertNotEqual(
            effective_metadata(expected), effective_metadata(different_plan)
        )
        for name, nullable in (("value!: i64", False), ("value?: i64", True)):
            record = self.record()
            record["describe"]["columns"][0]["name"] = name
            self.assertEqual(
                effective_metadata(record)["describe"]["nullable"][0], nullable
            )
        record["describe"]["columns"][0]["name"] = "value: i64"
        record["describe"]["nullable"][0] = True
        self.assertTrue(effective_metadata(record)["describe"]["nullable"][0])

    def test_missing_queries_types_and_sql_changes_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            old, new = root / "old", root / "new"
            old.mkdir()
            new.mkdir()
            (old / "query-fixture.json").write_text(json.dumps(self.record()))
            with self.assertRaises(ReleaseError):
                compare(old, new)
            mutations = [
                lambda r: r.update(query="different SQL"),
                lambda r: r["describe"]["columns"][0].update(type_info="Text"),
                lambda r: r["describe"].update(parameters={"Left": ["Text"]}),
                lambda r: r["describe"].update(nullable=[False, True, True]),
            ]
            for mutate in mutations:
                record = self.record()
                mutate(record)
                (new / "query-fixture.json").write_text(json.dumps(record))
                with self.assertRaises(ReleaseError):
                    compare(old, new)
            record = self.record()
            record["describe"]["nullable"] = [True, False, False]
            (new / "query-fixture.json").write_text(json.dumps(record))
            self.assertEqual(
                compare(old, new)["explicit_nullability_overrides"],
                ["query-fixture.json"],
            )
            (new / "query-extra.json").write_text(json.dumps(record))
            with self.assertRaises(ReleaseError):
                compare(old, new)
