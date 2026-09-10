"""SQLx checks tolerate overridden inference, never query/type changes or missing output."""

import copy
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from plamenu_release.core import ReleaseError
from plamenu_release.sqlx import compare, effective_metadata


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
