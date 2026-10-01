"""Real primary EIO/ENOSPC while the same native publisher keeps encoding.

Actual native JS/no-JS and PeerTube decoders span the fault. Only a uniquely
owned FUSE mount, origin, broadcasts and subscriber are affected; the backing
media and diagnostics are retained. FUSE is an opt-in test tool, never runtime.
"""
import errno
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import tempfile
import time
import tomllib

import pytest
import requests
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for
from tests.test_video_backup_restore import _csrf, _digest, _stop_process
from tests.test_video_delivery_scale import _children, _process
from tests.test_video_live_concurrency import _clock_publisher, _port
from tests.test_video_live_backup_restore import _stop_encoder
from tests.test_video_offload import _freeze
from tools.primary_io_fixture import PrimaryFilesystem


def _muxers(origin):
    found = []
    for pid in _children(origin._proc.pid):
        try:
            args = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
            if Path(f"/proc/{pid}/comm").read_text().strip() == "ffmpeg" and b"tee" in args:
                found.append(_process(pid)["identity"])
        except FileNotFoundError:
            continue
    return found


def _stage(browser, evidence, name):
    if browser.poll() is not None: raise RuntimeError("disk-fault browser exited; inspect its private log")
    report = evidence/"disk-browser.json"
    if not report.exists(): return False
    return json.loads(report.read_text()).get("stage") == name


@pytest.mark.skipif(os.environ.get("AP09_LIVE_DISK_VERIFY") != "1", reason="explicit real primary filesystem fault drill")
@pytest.mark.federation(direction="outbound",peer="peertube",
    one_way_reason="actual PeerTube decodes the same native broadcast throughout primary I/O failure")
@pytest.mark.parametrize("fault",["ENOSPC","EIO"])
def test_live_publisher_and_three_decoders_survive_primary_io_failure(fault,monkeypatch):
    assert peertube.reachable(), "actual PeerTube is required"
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1",8422)) != 0, "owned origin port is occupied"
    label = unique("liveio").replace("-","_")
    parent = config.PLAMENU_DIR/"target/e2e/video-live-disk"
    parent.mkdir(parents=True,exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-{fault.lower()}-",dir=parent))
    evidence.chmod(0o700)
    origin = ephemeral.Instance(domain=config.PLAMENU2_DOMAIN,bind="0.0.0.0:8422",db_name=f"plamenu_ap09_{label}")
    filesystem = publisher = observer = channel = api = pt = None
    subscribed = False
    probe = None
    observer_log = None
    session = requests.Session()
    session.verify = False
    try:
        with step("configure only an owned primary fault mount and independently durable replica"):
            origin.start()
            candidate = _freeze(origin,evidence,monkeypatch)
            _stop_process(origin)
            filesystem = PrimaryFilesystem(evidence/"primary-filesystem")
            replica = evidence/"replica:disk|safe"
            replica.mkdir(mode=0o700)
            settings = tomllib.loads(origin._config_path.read_text())
            rtmp_port = _port()
            text = origin._config_path.read_text().replace(json.dumps(settings["media_dir"]),json.dumps(str(filesystem.mount)))
            text = text.replace('live_ingest_bind = "127.0.0.1:0"',f'live_ingest_bind = "127.0.0.1:{rtmp_port}"')
            text += f'\n[media_mirror]\ndirectory={json.dumps(str(replica))}\nbandwidth_mib_per_second=32\nmin_free_gib=1\n'
            origin._config_path.write_text(text)
            origin._config_path.chmod(0o600)
            origin._launch(120,append_log=True)
            origin.apply_suite_settings()
            user, api = origin.user_api("resilientcaster")
            channel = api.post("/api/v1/video_channels",username=unique("resilientlive")[:30],display_name="Resilient publisher")
            pt = peertube.root()
            handle = f"{channel['username']}@{origin.domain}"
            pt.post("/api/v1/users/me/subscriptions",uri=handle)
            subscribed = True
            wait_for(lambda: origin.db.follower_count(channel["username"])>0,desc="actual PeerTube subscribes to owned channel")
            assert session.post(origin.url+"/login",data={"email":user.email,"password":user.password},allow_redirects=False,timeout=30).status_code == 303
            title = f"Surviving primary {fault} {label}"
            created = session.post(origin.url+"/web/videos/live",data={"csrf":_csrf(session.get(origin.url+"/videos",timeout=30).text),
                "channel":channel["id"],"title":title,"description":"Moving pixel clock and steady tone", "visibility":"public",
                "permanent":"yes","latency":"balanced","dvr_minutes":"5"},timeout=30)
            assert created.status_code == 200
            status = re.search(r'/videos/live/(\d+)',created.text).group(1)
            server, key, _ = re.findall(r'<input type="text" readonly value="([^"]+)"',created.text)
            media = int(api.get_status(status)["media_attachments"][0]["id"])
            publisher = _clock_publisher(server,key)
            wait_for(lambda: origin.db_value("SELECT live_state='live' FROM media_attachments WHERE id=%s",media),desc="owned publisher is on air",timeout=45)
            generation = origin.db_value("SELECT session_id FROM native_live_sessions WHERE media_id=%s AND ended_at IS NULL",media)
            durable = replica/"native-live"/str(media)/generation
            wait_for(lambda: (durable/"index.durable.m3u8").exists(),desc="independent replica acknowledges closed HLS and recording",timeout=30)
            initial = _muxers(origin)
            assert len(initial) == 1, "one native ingest encoder, not an extra replication encode"
            remote = wait_for(lambda: next((video for video in pt.get("/api/v1/users/me/subscriptions/videos",count=100,sort="-publishedAt")["data"]
                if video["name"] == title),None),desc="actual PeerTube receives owned live publication")
            manifest = evidence/"viewers.json"
            manifest.write_text(json.dumps({"local":f"{origin.url}/web/statuses/{status}","remote":f"{config.PEERTUBE_URL}/w/{remote['uuid']}"}))
            observer_log = (evidence/"browser.log").open("w")
            observer = subprocess.Popen(["node",str(config.PLAMENU_DIR/"accessibility/verify-live-disk-continuity.mjs"),str(manifest),str(evidence)],
                stdout=observer_log,stderr=subprocess.STDOUT,
                env={**os.environ,"PEERTUBE_E2E_USER":peertube.ROOT_USER,"PEERTUBE_E2E_PASSWORD":peertube.ROOT_PASS})

        with step(f"actual primary {fault} leaves the same encoder, committed replica and three real decoders progressing"):
            wait_for(lambda: _stage(observer,evidence,"ready-for-fault"),desc="three decoders establish the healthy baseline",timeout=90)
            before = int((durable/".plamenu-recording-offset").read_text())
            probe = os.open(filesystem.mount/"owned-writer-probe",os.O_CREAT|os.O_WRONLY,0o600)
            os.write(probe,b"before")
            filesystem.set_fault(fault)
            with pytest.raises(OSError) as actual: os.write(probe,b"must-not-write")
            assert actual.value.errno == getattr(errno,fault)
            (evidence/"fault-active.json").write_text(json.dumps({"errno":actual.value.errno,"fault":fault,"same_muxer":initial}))
            wait_for(lambda: _stage(observer,evidence,"ready-for-repair"),desc="all decoders sustain the real fault",timeout=45)
            assert publisher.poll() is None
            assert _muxers(origin) == initial
            assert origin.db_value("SELECT live_state='live' FROM media_attachments WHERE id=%s",media)
            after = int((durable/".plamenu-recording-offset").read_text())
            assert after > before
            # Keep the actual failed primary recording and acknowledged
            # replica prefix before normal owned-publication cleanup runs.
            shutil.copyfile(filesystem.backing/"native-live"/str(media)/generation/"replay.mp4",evidence/"failed-primary-recording.mp4")
            with (durable/"replay.mp4").open("rb") as current, (evidence/"committed-replica-prefix.mp4").open("wb") as retained:
                remaining = after
                while remaining:
                    chunk = current.read(min(65536,remaining))
                    assert chunk
                    retained.write(chunk)
                    remaining -= len(chunk)
            # The finite server-only clip must also use the surviving disk,
            # not a healthy-URL illusion backed by the failed primary.
            rewind = requests.get(f"{origin.url}/media/live/{media}/rewind/{int(time.time())//60}/0.mp4",verify=False,timeout=30)
            assert rewind.status_code == 200
            clip = evidence/"replica-during-fault.mp4"
            clip.write_bytes(rewind.content)
            subprocess.run(["ffmpeg","-nostdin","-v","error","-xerror","-i",str(clip),"-map","0:v:0","-map","0:a:0","-f","null","-"],
                check=True,capture_output=True,timeout=60)
            assert not list(durable.glob(".plamenu-rewind-*"))
            filesystem.set_fault(None)
            os.close(probe)
            probe = None
            (evidence/"fault-repaired.json").write_text(json.dumps({"recording_before":before,"recording_after":after}))
            assert observer.wait(timeout=35) == 0, "inspect private decoder log/phase JSON"

        with step("full surviving recording becomes an independently decoded PeerTube replay after primary repair"):
            _stop_encoder(publisher)
            publisher = None
            def replay_ready():
                return origin.db_value("SELECT m.status_id FROM native_live_sessions s JOIN media_attachments m ON m.id=s.replay_media_id "
                    "WHERE s.session_id=%s AND m.processing='complete'",generation)
            replay_status = wait_for(replay_ready,desc="surviving longest recording publishes its independent replay",timeout=240)
            replay = api.get_status(replay_status)["media_attachments"][0]
            assert replay["meta"]["original"]["duration"] >= 38
            content = requests.get(replay["url"],verify=False,timeout=30)
            content.raise_for_status()
            movie = evidence/"surviving-replay.mp4"
            movie.write_bytes(content.content)
            subprocess.run(["ffmpeg","-nostdin","-v","error","-xerror","-i",str(movie),"-map","0:v:0","-map","0:a:0","-f","null","-"],
                check=True,capture_output=True,timeout=90)
            replay_uuid = origin.db_value("SELECT uuid::text FROM video_publications WHERE media_id=%s",int(replay["id"]))
            imported = wait_for(lambda: pt.video(replay_uuid) if pt.video(replay_uuid)["files"] else None,desc="actual PeerTube receives surviving replay")
            ranged = requests.get(imported["files"][0]["fileUrl"],headers={"Range":"bytes=17-80"},verify=False,timeout=30)
            assert ranged.status_code == 206 and ranged.content == content.content[17:81]
            result = {"candidate_sha256":candidate,"fault":fault,"actual_errno":actual.value.errno,"encoder_identity":initial,
                "same_encoder_and_session":True,"committed_recording_before":before,"committed_recording_after":after,
                "decoded_browsers":json.loads((evidence/"disk-browser.json").read_text()),
                "replay_uuid":replay_uuid,"replay_sha256":_digest(movie),"replay_duration":replay["meta"]["original"]["duration"]}
            (evidence/"result.json").write_text(json.dumps(result,indent=2))
            print(f"Protected real primary I/O fault evidence: {evidence}")
    finally:
        if filesystem is not None: filesystem.set_fault(None)
        if probe is not None: os.close(probe)
        if observer is not None and observer.poll() is None:
            observer.terminate()
            observer.wait(timeout=15)
        if observer_log is not None: observer_log.close()
        _stop_encoder(publisher)
        try:
            if origin._proc is not None and origin._proc.poll() is None and channel is not None:
                api.post(f"/api/v1/video_channels/{channel['id']}",operation="delete",confirm=channel["username"])
            if subscribed: pt.delete(f"/api/v1/users/me/subscriptions/{handle}")
        finally:
            if origin._dir is not None:
                log = Path(origin._dir.name)/"server.log"
                if log.exists(): (evidence/"origin.log").write_text(log.read_text())
            origin.stop()
            session.close()
            if filesystem is not None: filesystem.stop()
            for path in evidence.rglob("*"):
                if path.is_file(): path.chmod(0o700 if path.name in ("candidate-plamenu","primary-io-fault") else 0o600)
