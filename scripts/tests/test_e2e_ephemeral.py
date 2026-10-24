"""Real socket ownership and early refusal protect disposable test databases."""

import importlib.util
import os
import socket
import subprocess
import sys
import tempfile
import types
import unittest
from contextlib import contextmanager
from pathlib import Path
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "e2e"))
from plamenu_e2e import candidate


def load_owner():
    stubs = {
        name: types.ModuleType(name)
        for name in (
            "psycopg",
            "requests",
            "plamenu_e2e.plamenu",
            "plamenu_e2e.api",
            "plamenu_e2e.db",
        )
    }
    stubs["psycopg"].sql = MagicMock()
    stubs["psycopg"].connect = MagicMock()
    stubs["psycopg"].Error = RuntimeError
    stubs["requests"].get = MagicMock(return_value=types.SimpleNamespace(ok=True))
    stubs["requests"].RequestException = RuntimeError
    stubs["plamenu_e2e.plamenu"].Cli = object
    stubs["plamenu_e2e.api"].Api = object
    stubs["plamenu_e2e.db"].Db = object
    spec = importlib.util.spec_from_file_location(
        "plamenu_e2e._ephemeral_guard_tests", ROOT / "e2e/plamenu_e2e/ephemeral.py"
    )
    owner = importlib.util.module_from_spec(spec)
    with patch.dict(sys.modules, stubs):
        spec.loader.exec_module(owner)
    return owner


class EphemeralTests(unittest.TestCase):
    def setUp(self):
        self.owner = load_owner()

    def test_scalar_error_is_not_retried_and_releases_the_failed_session(self):
        instance = self.owner.Instance()
        failed = MagicMock(closed=False)
        failed.execute.side_effect = RuntimeError("observation failed")
        recovered = MagicMock(closed=False)
        recovered.execute.return_value.fetchone.return_value = (42,)
        self.owner.psycopg.connect.side_effect = [failed, recovered]
        with self.assertRaisesRegex(RuntimeError, "observation failed"):
            instance.db_value("SELECT broken")
        failed.execute.assert_called_once()
        failed.close.assert_called_once()
        self.assertIsNone(instance._scalar_connection)
        self.assertEqual(instance.db_value("SELECT value"), 42)
        instance.stop()
        recovered.close.assert_called_once()

    def test_scalar_session_is_rebound_when_the_database_changes(self):
        instance = self.owner.Instance()
        first, second = MagicMock(closed=False), MagicMock(closed=False)
        first.execute.return_value.fetchone.return_value = (1,)
        second.execute.return_value.fetchone.return_value = (2,)
        self.owner.psycopg.connect.side_effect = [first, second]
        self.assertEqual(instance.db_value("SELECT value"), 1)
        instance.dsn = "postgres://owned/other_fixture"
        self.assertEqual(instance.db_value("SELECT value"), 2)
        first.close.assert_called_once()
        self.assertEqual(self.owner.psycopg.connect.call_args.args, (instance.dsn,))
        instance.stop()
        second.close.assert_called_once()

    def test_scalar_value_cannot_pass_when_session_cleanup_fails(self):
        instance = self.owner.Instance()
        connection = MagicMock(closed=False)
        result = MagicMock()
        result.fetchone.return_value = (42,)
        connection.execute.side_effect = [result, RuntimeError("session reset failed")]
        self.owner.psycopg.connect.return_value = connection
        with self.assertRaisesRegex(RuntimeError, "session reset failed"):
            instance.db_value("SELECT value")
        self.assertIsNone(instance._scalar_connection)
        connection.close.assert_called_once()
        self.assertEqual(self.owner.psycopg.connect.call_count, 1)

    def test_scalar_backend_closes_before_database_drop_even_if_other_cleanup_fails(
        self,
    ):
        instance = self.owner.Instance()
        events = []
        scalar = MagicMock()
        scalar.close.side_effect = lambda: events.append("scalar")
        instance._scalar_connection = scalar
        instance._database_owned = True
        instance._db = MagicMock()

        def failed_close():
            events.append("db")
            raise RuntimeError("other connection close failed")

        instance._db.close.side_effect = failed_close
        with patch.object(
            instance, "_drop_database", side_effect=lambda: events.append("drop")
        ):
            with self.assertRaisesRegex(RuntimeError, "other connection close failed"):
                instance.stop()
            self.assertEqual(events, ["scalar", "db", "drop"])
            self.assertIsNone(instance._scalar_connection)
            self.assertFalse(instance._database_owned)
            instance.stop()
            self.assertEqual(events, ["scalar", "db", "drop"])

    def test_scalar_call_on_another_thread_never_borrows_the_owners_session(self):
        instance = self.owner.Instance()
        owned = MagicMock()
        instance._scalar_connection = owned
        short = self.owner.psycopg.connect.return_value
        short.__enter__.return_value.execute.return_value.fetchone.return_value = (7,)
        with patch.object(
            self.owner.threading, "get_ident", return_value=instance._scalar_thread + 1
        ):
            self.assertEqual(instance.db_value("SELECT value"), 7)
            self.assertEqual(instance.db_value("SELECT value"), 7)
        self.assertEqual(self.owner.psycopg.connect.call_count, 2)
        self.assertEqual(short.__exit__.call_count, 2)
        owned.execute.assert_not_called()
        self.assertIs(instance._scalar_connection, owned)
        instance.stop()
        owned.close.assert_called_once()

    def test_failed_launch_drops_only_new_database_and_releases_port_lease(self):
        with tempfile.TemporaryDirectory() as directory:
            instance = self.owner.Instance(bind="127.0.0.1:0")
            with (
                patch.object(candidate, "prepare_binary"),
                patch.object(self.owner.config, "PLAMENU_DIR", Path(directory)),
                patch.object(
                    self.owner, "binary_path", return_value=Path(sys.executable)
                ),
                self.assertRaisesRegex(RuntimeError, "did not come up"),
            ):
                instance.start(timeout=1)
            self.assertIsNone(instance._proc)
            self.assertIsNone(instance._dir)
            self.assertIsNone(instance._lease)
            self.assertFalse(instance._database_owned)
            self.assertEqual(
                list((Path(directory) / "target/e2e/instances").iterdir()), []
            )
            statements = [call.args[0] for call in self.owner.sql.SQL.call_args_list]
            self.assertEqual(
                statements,
                [
                    "CREATE DATABASE {} TEMPLATE template0",
                    "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                ],
            )
            calls = self.owner.psycopg.connect.call_count
            instance.stop()
            self.assertEqual(self.owner.psycopg.connect.call_count, calls)

    def test_database_collision_never_drops_existing_database(self):
        instance = self.owner.Instance(bind="127.0.0.1:0", db_name="preexisting")
        self.owner.psycopg.connect.return_value.__enter__.return_value.execute.side_effect = RuntimeError(
            "already exists"
        )
        with (
            patch.object(candidate, "prepare_binary"),
            self.assertRaisesRegex(RuntimeError, "already exists"),
        ):
            instance.start()
        statements = [call.args[0] for call in self.owner.sql.SQL.call_args_list]
        self.assertEqual(statements, ["CREATE DATABASE {} TEMPLATE template0"])
        self.assertIsNone(instance._lease)

    def test_default_database_names_are_unique(self):
        instances = [
            self.owner.Instance(),
            self.owner.Instance(),
            self.owner.peer(),
            self.owner.peer(),
        ]
        self.assertEqual(len({instance.db_name for instance in instances}), 4)

    def test_only_ordinary_startup_can_use_the_session_schema(self):
        with patch.object(
            self.owner.schema_template, "active_name", return_value="owned_schema"
        ) as name:
            restore = self.owner.Instance()
            restore.create_database()
            name.assert_not_called()
            ordinary = self.owner.Instance()
            ordinary.create_database(migrated=True)
            name.assert_called_once()
        self.assertEqual(
            [call.args[0] for call in self.owner.sql.SQL.call_args_list],
            ["CREATE DATABASE {} TEMPLATE template0", "CREATE DATABASE {} TEMPLATE {}"],
        )

    def test_suite_settings_are_seeded_by_real_cli_before_first_server_launch(self):
        for enabled in (False, True):
            with (
                self.subTest(enabled=enabled),
                tempfile.TemporaryDirectory() as directory,
            ):
                instance = self.owner.Instance(bind="127.0.0.1:0")
                events = []
                with (
                    patch.object(self.owner.config, "PLAMENU_DIR", Path(directory)),
                    patch.object(instance, "create_database"),
                    patch.object(
                        instance,
                        "cli",
                        side_effect=lambda *args, events=events: events.append(
                            ("cli", args)
                        ),
                    ),
                    patch.object(
                        instance,
                        "apply_suite_settings",
                        side_effect=lambda events=events: events.append("settings"),
                    ),
                    patch.object(
                        instance,
                        "_launch",
                        side_effect=lambda *args, events=events, **kw: events.append(
                            "serve"
                        ),
                    ),
                ):
                    instance._start(1, suite_settings=enabled)
                    self.assertEqual(
                        events,
                        [("cli", ("role", "list")), "settings", "serve"]
                        if enabled
                        else ["serve"],
                    )
                    instance._dir.cleanup()
                    instance._dir = None

    def test_settings_cannot_touch_unowned_or_running_instances_or_hide_missing_row(
        self,
    ):
        instance = self.owner.Instance()
        with self.assertRaisesRegex(RuntimeError, "owned database"):
            instance.apply_suite_settings()
        self.owner.psycopg.connect.assert_not_called()
        instance._database_owned = True
        instance._proc = types.SimpleNamespace(poll=lambda: None)
        with self.assertRaisesRegex(RuntimeError, "stopped server"):
            instance.apply_suite_settings()
        self.owner.psycopg.connect.assert_not_called()
        instance._proc = None
        result = self.owner.psycopg.connect.return_value.__enter__.return_value.execute.return_value
        result.fetchone.return_value = None
        with self.assertRaisesRegex(RuntimeError, "no settings row"):
            instance.apply_suite_settings()
        result.fetchone.return_value = (True,)
        with patch.object(self.owner.time, "sleep") as sleep:
            instance.apply_suite_settings()
        sleep.assert_not_called()

    def test_restore_database_is_owned_even_if_restore_fails_before_launch(self):
        instance = self.owner.Instance()
        instance.create_database()
        self.assertTrue(instance._database_owned)
        instance.stop()
        self.assertFalse(instance._database_owned)
        self.assertEqual(
            [call.args[0] for call in self.owner.sql.SQL.call_args_list],
            [
                "CREATE DATABASE {} TEMPLATE template0",
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            ],
        )

    def test_listener_handoff_requires_dead_source_and_keeps_its_data(self):
        server = subprocess.Popen(
            [
                sys.executable,
                "-c",
                (
                    "import socket,time; s=socket.socket(); s.bind(('127.0.0.1',0)); "
                    "s.listen(1); print(s.getsockname()[1],flush=True); time.sleep(30)"
                ),
            ],
            stdout=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        self.addCleanup(server.stdout.close)
        self.addCleanup(lambda: self.owner.stop_process(server))
        port = int(server.stdout.readline())
        instance = self.owner.Instance(bind=f"127.0.0.1:{port}")
        instance._proc = server
        instance._lease = self.owner.Lease(f"e2e-listener:{port}")
        self.addCleanup(lambda: instance._lease and instance._lease.close())
        with self.assertRaisesRegex(RuntimeError, "running fixture"):
            instance.release_listener_after_exit()
        with self.assertRaisesRegex(RuntimeError, "already owned"):
            self.owner.Lease(f"e2e-listener:{port}")
        self.owner.stop_process(server)
        instance._database_owned = True
        instance.release_listener_after_exit()
        self.assertTrue(instance._database_owned)
        self.owner.psycopg.connect.assert_not_called()
        with self.owner.Lease(f"e2e-listener:{port}"):
            pass

    def test_listener_handoff_cannot_release_a_foreign_listener(self):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            port = listener.getsockname()[1]
            instance = self.owner.Instance(bind=f"127.0.0.1:{port}")
            instance._lease = self.owner.Lease(f"e2e-listener:{port}")
            self.addCleanup(instance._lease.close)
            with self.assertRaisesRegex(RuntimeError, "occupied"):
                instance.release_listener_after_exit()
            with self.assertRaisesRegex(RuntimeError, "already owned"):
                self.owner.Lease(f"e2e-listener:{port}")

    def test_occupied_port_refuses_before_database_reset(self):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            instance = self.owner.Instance(
                bind=f"127.0.0.1:{listener.getsockname()[1]}"
            )
            with (
                patch.object(candidate, "prepare_binary"),
                self.assertRaisesRegex(RuntimeError, "occupied"),
            ):
                instance.start()
            self.owner.psycopg.connect.assert_not_called()

    def test_readiness_requires_own_child_socket_not_foreign_healthy_http(self):
        server = subprocess.Popen(
            [
                sys.executable,
                "-c",
                (
                    "import socket,time; s=socket.socket(); s.bind(('127.0.0.1',0)); "
                    "s.listen(1); print(s.getsockname()[1],flush=True); time.sleep(30)"
                ),
            ],
            stdout=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(server.stdout.close)
        self.addCleanup(lambda: server.wait(timeout=5))
        self.addCleanup(server.terminate)
        port = int(server.stdout.readline())
        instance = self.owner.Instance(bind=f"127.0.0.1:{port}")
        instance._proc = server
        self.assertTrue(instance._owns_listener())
        with tempfile.TemporaryDirectory() as directory:
            instance._dir = types.SimpleNamespace(name=directory)
            instance.bind = "0.0.0.0:8422"
            instance._config_path.write_text(f'bind = "127.0.0.1:{port}"\n')
            self.assertTrue(instance._owns_listener())
            instance._config_path.write_text('bind = "127.0.0.1:0"\n')
            self.assertFalse(instance._owns_listener())
        instance._dir = None
        instance._proc = types.SimpleNamespace(pid=os.getpid(), poll=lambda: None)
        self.assertFalse(instance._ready())
        self.owner.requests.get.assert_not_called()
        self.owner.psycopg.connect.assert_not_called()

    def test_empty_schema_cannot_be_ready_despite_healthy_proxy(self):
        instance = self.owner.Instance()
        instance._proc = types.SimpleNamespace(pid=os.getpid(), poll=lambda: None)
        self.owner.psycopg.connect.return_value.__enter__.return_value.execute.return_value.fetchone.return_value = (
            False,
        )
        with patch.object(instance, "_owns_listener", return_value=True):
            self.assertFalse(instance._ready())
        self.owner.requests.get.assert_not_called()


class LaunchReadinessTests(unittest.TestCase):
    @contextmanager
    def launch(self):
        owner = load_owner()
        instance = owner.Instance()
        clock = types.SimpleNamespace(now=0.0, waits=[])

        def sleep(delay):
            clock.waits.append(delay)
            clock.now += delay

        process = MagicMock()
        process.poll.return_value = None
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(owner, "binary_path", return_value=Path("/unused/candidate")),
            patch.object(owner.subprocess, "Popen", return_value=process),
            patch.object(owner.time, "monotonic", side_effect=lambda: clock.now),
            patch.object(owner.time, "sleep", side_effect=sleep),
            patch.object(instance, "_ready") as ready,
            patch.object(instance, "log_tail", return_value="readiness diagnostic"),
        ):
            instance._dir = types.SimpleNamespace(name=directory)
            instance._lease = MagicMock()
            try:
                yield instance, ready, process, clock
            finally:
                if instance._log is not None:
                    instance._log.close()

    def test_fast_start_still_requires_a_successful_readiness_observation(self):
        with self.launch() as (instance, ready, _, clock):
            ready.side_effect = [False, True]
            instance._launch_process(120, append_log=False)
            self.assertEqual(ready.call_count, 2)
            self.assertLess(clock.now, 0.5)

    def test_slow_start_and_transient_error_keep_the_full_readiness_budget(self):
        with self.launch() as (instance, ready, _, clock):
            observations = []

            def eventually_ready():
                observations.append(clock.now)
                if len(observations) == 1:
                    raise RuntimeError("transient connection error")
                return clock.now >= 119.5

            ready.side_effect = eventually_ready
            instance._launch_process(120, append_log=True)
            self.assertGreaterEqual(clock.now, 119.5)
            self.assertLess(clock.now, 120)
            self.assertTrue(all(delay == 0.5 for delay in clock.waits[10:]))
            self.assertLessEqual(len(observations), 250)

    def test_timeout_and_exited_child_cannot_become_ready(self):
        for exited in (False, True):
            with (
                self.subTest(exited=exited),
                self.launch() as (
                    instance,
                    ready,
                    process,
                    clock,
                ),
            ):
                ready.return_value = False
                if exited:
                    process.poll.side_effect = [None, 17]
                with self.assertRaisesRegex(
                    RuntimeError, "(?s)did not come up; log tail:.*readiness diagnostic"
                ):
                    instance._launch_process(0.02, append_log=False)
                self.assertLessEqual(clock.now, 0.02)
                self.assertEqual(ready.call_count, 1)


if __name__ == "__main__":
    unittest.main()
