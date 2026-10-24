"""Resource evidence must survive a real process gap and surface sampler errors."""

import os
from types import SimpleNamespace
from unittest.mock import Mock

import pytest
from plamenu_e2e.resources import Resources


def process():
    return SimpleNamespace(pid=os.getpid(), poll=lambda: None)


def test_sampling_resumes_after_the_crash_restart_process_gap(tmp_path):
    media = tmp_path / "media"
    media.mkdir()
    (media / "sample").write_bytes(b"owned media")
    instance = SimpleNamespace(_proc=None, _dir=SimpleNamespace(name=tmp_path))
    resources = Resources(instance, connect=Mock())

    class RestartBoundary:
        waits = 0

        def is_set(self):
            return self.waits == 2

        def wait(self, timeout):
            self.waits += 1
            instance._proc = process()

    resources.stop = RestartBoundary()
    connection = Mock()
    connection.execute.return_value.fetchone.return_value = (1, 1)
    resources.sample_process(connection)
    assert resources.stop.waits == 2
    assert resources.parent_peak_kib > 0
    assert resources.disk_peak == len(b"owned media")
    assert resources.jobs_peak == resources.uploaders_peak == 1


def test_sampler_failure_cannot_leave_passing_resource_evidence(tmp_path):
    instance = SimpleNamespace(
        _proc=process(), _dir=SimpleNamespace(name=tmp_path), dsn="unused"
    )
    failure = RuntimeError("database sampling failed")
    with (
        pytest.raises(RuntimeError, match="resource sampler failed") as caught,
        Resources(instance, connect=Mock(side_effect=failure)) as resources,
    ):
        resources.thread.join(timeout=1)
    assert caught.value.__cause__ is failure
    assert not resources.thread.is_alive()
