"""Real socket ownership and early refusal protect disposable test databases."""

import importlib.util
import os
import socket
import subprocess
import sys
import tempfile
import types
import unittest
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


if __name__ == "__main__":
    unittest.main()
