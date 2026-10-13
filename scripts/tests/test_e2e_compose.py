"""Fixture commands must use the running project's actual database namespace."""

import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "e2e"))
from plamenu_e2e import shell


class ComposeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.fixture = self.root / "sharkey-test"
        self.fixture.mkdir()
        self.alias = self.root / "preserved-fleet"
        self.alias.symlink_to(self.fixture, target_is_directory=True)
        shell.compose_project.cache_clear()
        self.addCleanup(shell.compose_project.cache_clear)

    def labels(self, project, directory, service="db"):
        return json.dumps(
            {
                "com.docker.compose.project": project,
                "com.docker.compose.project.working_dir": str(directory),
                "com.docker.compose.service": service,
            }
        )

    def test_preserved_symlink_namespace_selects_actual_running_project(self):
        with (
            patch.object(shell, "docker", side_effect=lambda *args: ["docker", *args]),
            patch.object(
                shell,
                "run",
                side_effect=[
                    "a b",
                    self.labels("ap09-latest-sharkey", self.alias)
                    + "\n"
                    + self.labels("unrelated", self.root),
                    "result",
                ],
            ) as run,
        ):
            self.assertEqual(
                shell.fixture_compose(self.fixture, "exec", "-T", "db", "psql"),
                "result",
            )
            command = run.call_args_list[-1].args[0]
            self.assertIn("--project-name", command)
            self.assertIn("ap09-latest-sharkey", command)
            self.assertNotIn("unrelated", command)

    def test_ambiguous_projects_cannot_execute_database_commands(self):
        with (
            patch.object(shell, "docker", side_effect=lambda *args: ["docker", *args]),
            patch.object(
                shell,
                "run",
                side_effect=[
                    "a b",
                    self.labels("old", self.fixture)
                    + "\n"
                    + self.labels("new", self.alias),
                ],
            ) as run,
        ):
            with self.assertRaisesRegex(RuntimeError, "Ambiguous"):
                shell.fixture_compose(self.fixture, "exec", "db", "psql")
            self.assertEqual(run.call_count, 2)

    def test_proxy_and_application_projects_in_same_directory_are_disambiguated_by_service(
        self,
    ):
        with (
            patch.object(shell, "docker", side_effect=lambda *args: ["docker", *args]),
            patch.object(
                shell,
                "run",
                side_effect=[
                    "a b",
                    self.labels("proxy", self.fixture, "caddy")
                    + "\n"
                    + self.labels("runtime", self.fixture, "web"),
                    "result",
                ],
            ) as run,
        ):
            shell.fixture_compose(
                self.fixture,
                "run",
                "--rm",
                "-T",
                "web",
                "bin/rails",
                "runner",
                "puts :ok",
            )
            self.assertIn("runtime", run.call_args_list[-1].args[0])
            self.assertNotIn("proxy", run.call_args_list[-1].args[0])

    def test_no_running_fixture_retains_declared_project_for_startup(self):
        with (
            patch.object(shell, "docker", side_effect=lambda *args: ["docker", *args]),
            patch.object(shell, "run", side_effect=["", "started"]) as run,
        ):
            self.assertEqual(shell.fixture_compose(self.fixture, "up", "-d"), "started")
            self.assertNotIn("--project-name", run.call_args_list[-1].args[0])


if __name__ == "__main__":
    unittest.main()
