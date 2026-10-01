"""Two native DB-less processors, bounded private leases and real PeerTube outputs.

Only the uniquely owned origin, worker processes/roots and created channel are
changed. Credentials stay in protected configuration, never command arguments.
"""
import concurrent.futures
import hashlib
import json
import os
import re
import secrets
import shutil
import socket
import subprocess
import tempfile
import time
import tomllib
from pathlib import Path
from urllib.parse import urlsplit

import pytest
import psycopg
import requests
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for
from tests.test_video_backup_restore import _csrf, _digest, _stop_process


def _freeze(origin, evidence, monkeypatch):
    # Follow the running inode, not a pathname another legitimate build can
    # atomically replace during a long crash/recovery drill.
    frozen = evidence/"candidate-plamenu"
    shutil.copyfile(f"/proc/{origin._proc.pid}/exe",frozen)
    frozen.chmod(0o700)
    monkeypatch.setattr(ephemeral,"BINARY",frozen)
    return _digest(frozen)


class Processor:
    def __init__(self, root, *, tls=False):
        self.root = root
        root.mkdir(mode=0o700)
        self.config = root / "worker.toml"
        subprocess.run([str(ephemeral.BINARY), "--config", str(self.config), "config", "generate", "--video-worker"],
                       check=True, capture_output=True, timeout=30)
        self.token = tomllib.loads(self.config.read_text())["token"]
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            self.port = sock.getsockname()[1]
        text = self.config.read_text().replace('"127.0.0.1:8423"', f'"127.0.0.1:{self.port}"')
        text = text.replace('"./video-worker"', json.dumps(str(root / "scratch")))
        text += '\n[native_video]\nmax_upload_mib = 512\njobs = 1\n'
        self.url = f"http://127.0.0.1:{self.port}"
        if tls:
            # A fixture leaf, not the CA private key or federation keys. Native
            # origin reqwest must validate this already-trusted local CA chain.
            for extension in ("crt", "key"):
                destination = root / f"worker.{extension}"
                subprocess.run(["docker", "cp", f"mastodon-test-caddy-1:/data/caddy/certificates/local/plamenu2.local/plamenu2.local.{extension}", str(destination)],
                               check=True, capture_output=True, timeout=15)
                destination.chmod(0o600)
            text += f'\n[tls]\ncert = {json.dumps(str(root / "worker.crt"))}\nkey = {json.dumps(str(root / "worker.key"))}\n'
            self.url = f"https://{config.PLAMENU2_DOMAIN}:{self.port}"
        self.config.write_text(text)
        self.config.chmod(0o600)
        self.proc = None
        try:
            self.launch()
        except BaseException:
            self.stop()
            raise

    def launch(self):
        self.log = (self.root / "worker.log").open("a")
        self.proc = subprocess.Popen([str(ephemeral.BINARY), "--config", str(self.config), "video-worker"],
                                     stdout=self.log, stderr=subprocess.STDOUT, env={**os.environ, "RUST_LOG":"info"})
        wait_for(lambda: self.health_or_none(), desc="native DB-less processor listens", timeout=30)

    def request(self, method, path, *, authorized=True, **kwargs):
        headers = kwargs.pop("headers", {})
        if authorized:
            headers["Authorization"] = f"Bearer {self.token}"
        return requests.request(method, self.url + path, headers=headers, verify=False, timeout=15, **kwargs)

    def health_or_none(self):
        try:
            if self.proc.poll() is not None:
                raise RuntimeError("private processor exited; inspect its protected log")
            response = self.request("GET", "/v1/health")
            return response.json() if response.status_code == 200 else None
        except requests.ConnectionError:
            return None

    def stop(self, *, crash=False):
        if self.proc is not None and self.proc.poll() is None:
            self.proc.kill() if crash else self.proc.terminate()
            self.proc.wait(timeout=15)
        if getattr(self,"log",None) is not None and not self.log.closed:
            self.log.close()


def _spec(*, source_bytes=10):
    return {"version":1,"stem":"1"+secrets.token_hex(16),"source_bytes":source_bytes,
            "output_bytes":64*1024*1024,"preset":"veryfast","crf":23,"audio_kbps":192,
            "force_reencode":False,"window_ms":None,"poster_seconds":0,
            "avif_quality":70,"avif_speed":6}


def _submit(origin, user, channel, source, title):
    session = requests.Session()
    session.verify = False
    assert session.post(origin.url+"/login", data={"email":user.email,"password":user.password},
                        allow_redirects=False, timeout=30).status_code == 303
    csrf = _csrf(session.get(origin.url+"/videos",timeout=30).text)
    fields = {"csrf":csrf,"channel":channel["id"],"title":title,"description":"Moving pattern and test tone.",
              "visibility":"public","language":"en","alt":"Moving coloured test pattern.","op":"review"}
    with source.open("rb") as stream:
        response = session.post(origin.url+"/web/videos", files=[(key,(None,str(value))) for key,value in fields.items()]+
            [("file",(source.name,stream,"video/webm" if source.suffix == ".webm" else "video/mp4"))],allow_redirects=False,timeout=90)
    assert response.status_code == 303, response.status_code
    media = int(urlsplit(response.headers["Location"]).path.rsplit("/",1)[1])
    return media, session


def _ready(origin, media):
    wait_for(lambda: origin.db_value("SELECT state='ready' FROM video_drafts WHERE media_id=%s",media),
             desc="private offloaded draft reaches fenced ready state",timeout=240)


def _upload(origin, user, channel, source, title):
    media, _ = _submit(origin,user,channel,source,title)
    _ready(origin,media)
    return media


def _encoder(processor):
    # Observe the actual native encoder, not an idle reservation or ffprobe.
    pending = [processor.proc.pid]
    while pending:
        parent = pending.pop()
        try:
            # Linux children belong to the spawning thread; Tokio need not
            # spawn FFmpeg from its process leader's thread.
            children = []
            for thread in Path(f"/proc/{parent}/task").iterdir():
                try:
                    children.extend((thread/"children").read_text().split())
                except FileNotFoundError:
                    continue
        except FileNotFoundError:
            continue
        for value in children:
            child = int(value)
            pending.append(child)
            try:
                args = Path(f"/proc/{child}/cmdline").read_bytes().split(b"\0")
                name = Path(f"/proc/{child}/comm").read_text().strip()
                if name == "ffmpeg" and (b"libx264" in args or b"h264" in args) and b"veryslow" in args \
                        and any(argument.endswith(b"/source.bin") for argument in args):
                    state = Path(f"/proc/{child}/stat").read_text().rsplit(") ",1)[1].split()
                    return child, state[19]  # PID plus start ticks, not a reused PID.
            except FileNotFoundError:
                continue
    return None


def _gone(process):
    pid, started = process
    try:
        state = Path(f"/proc/{pid}/stat").read_text().rsplit(") ",1)[1].split()
        return state[0] == "Z" or state[19] != started
    except FileNotFoundError:
        return True


def _source_path(origin, media):
    key = origin.db_value("SELECT source_file FROM video_drafts WHERE media_id=%s",media)
    matches = list((Path(origin._dir.name)/"media").rglob(key))
    assert len(matches) == 1, "origin must retain precisely its own source"
    return matches[0]


@pytest.mark.skipif(os.environ.get("AP09_OFFLOAD_VERIFY") != "1", reason="explicit native processor drill")
@pytest.mark.federation(direction="outbound",peer="peertube",
    one_way_reason="native private processing leaves canonical outbound Video assets unchanged")
def test_two_private_native_processors_peer_outputs_leases_and_worker_crash(monkeypatch):
    assert peertube.reachable(), "actual PeerTube 8.3 is required"
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1",8422)) != 0, "disposable origin port is occupied"
    artifacts = config.PLAMENU_DIR / "target/e2e/video-offload"
    artifacts.mkdir(parents=True,exist_ok=True)
    label = unique("offload").replace("-","_")
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-",dir=artifacts))
    evidence.chmod(0o700)
    origin = ephemeral.Instance(domain=config.PLAMENU2_DOMAIN,bind="0.0.0.0:8422",db_name=f"plamenu_ap09_{label}")
    processors = []
    channel = pt = None
    second_channel = None
    users = []
    followed = []
    receipts = {}
    try:
        origin.start()
        _freeze(origin,evidence,monkeypatch)
        _stop_process(origin)
        with step("same native binary starts two private workers without DB credentials, one with native TLS"):
            processors.append(Processor(evidence/"worker-a"))
            processors.append(Processor(evidence/"worker-b",tls=True))
            for processor in processors:
                assert "database_url" not in tomllib.loads(processor.config.read_text())
                assert processor.request("GET","/v1/health",authorized=False).status_code == 401
                assert processor.request("GET","/media/source.bin",authorized=False).status_code == 401
                assert processor.request("POST","/v1/jobs",json={**_spec(),"source_url":"file:///private"}).status_code == 422
                assert processor.health_or_none()["active_jobs"] == 0
            native = '\n[native_video]\nmax_upload_mib = 512\njobs = 2\n'
            targets = ''.join(f'\n[[video_offload]]\nurl = {json.dumps(processor.url+"/")}\ntoken = {json.dumps(processor.token)}\n' for processor in processors)
            origin._config_path.write_text(origin._config_path.read_text()+native+targets)
            origin._config_path.chmod(0o600)
            origin._launch(120,append_log=True)
            origin.apply_suite_settings()
            candidate = _digest(Path(f"/proc/{origin._proc.pid}/exe"))
            assert all(_digest(Path(f"/proc/{processor.proc.pid}/exe")) == candidate for processor in processors)
            users = [origin.user_api("offloadcreator") for _ in range(2)]
            channel = users[0][1].post("/api/v1/video_channels",username=unique("offloadfilms")[:30],display_name="Private processing films")
            second_channel = users[1][1].post("/api/v1/video_channels",username=unique("offloadother")[:30],display_name="Second processing films")
            pt = peertube.root()
            for item in [channel,second_channel]:
                pt.post("/api/v1/users/me/subscriptions",uri=f"{item['username']}@{origin.domain}")
                followed.append(item)
            for item in [channel,second_channel]:
                wait_for(lambda: origin.db.follower_count(item["username"])>0,desc="actual PeerTube follows owned channel")
            source = peertube.make_sample_mp4(evidence/"source.mp4",seconds=8,size="640x360")
            names = [f"Private native offload {label} A",f"Private native offload {label} B"]
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
                futures = [executor.submit(_upload,origin,user[0],item,source,name) for user,item,name in zip(users,[channel,second_channel],names)]
                mids = [future.result(timeout=240) for future in futures]
            receipts["workers"] = [processor.health_or_none() for processor in processors]
            assert all(record["completed_jobs"] >= 1 and record["active_jobs"] == 0 for record in receipts["workers"]), receipts["workers"]
            text = (Path(origin._dir.name)/"server.log").read_text()
            assert text.count("native video offload output verified") >= 2
            assert "private video processor unavailable or invalid" not in text, "positive case must not silently fall back to local encoding"
            receipts["published"] = []
            for (_,api),item,mid,name in zip(users,[channel,second_channel],mids,names):
                assert requests.get(f"{origin.url}/media/hls/{mid}/master.m3u8",verify=False,timeout=30).status_code == 404
                published = api.post("/api/v1/videos",channel_id=item["id"],media_id=str(mid),title=name,visibility="public")
                remote = wait_for(lambda: next((video for video in pt.get("/api/v1/users/me/subscriptions/videos",count=100,sort="-publishedAt")["data"] if video["name"]==name),None),desc="PeerTube imports offloaded native publication")
                details = pt.video(remote["uuid"])
                assert details["streamingPlaylists"]
                media = published["media_attachments"][0]
                response = requests.get(media["url"],verify=False,timeout=30)
                assert response.status_code == 200
                film = evidence/f"decoded-{mid}.mp4"
                film.write_bytes(response.content)
                subprocess.run(["ffmpeg","-nostdin","-v","error","-xerror","-i",str(film),"-map","0:v:0","-map","0:a:0","-f","null","-"],check=True,capture_output=True,timeout=60)
                ranged = requests.get(media["url"],headers={"Range":"bytes=0-63"},verify=False,timeout=30)
                assert ranged.status_code == 206 and ranged.content == response.content[:64]
                receipts["published"].append({"native_url":published["url"],"peertube_url":f"{config.PEERTUBE_URL}/w/{remote['uuid']}","sha256":hashlib.sha256(response.content).hexdigest(),"peer_uuid":remote["uuid"]})
            playback_manifest = evidence/"playback-manifest.json"
            playback_manifest.write_text(json.dumps(receipts["published"]))
            with (evidence/"browser.log").open("w") as browser_log:
                subprocess.run(["node",str(config.PLAMENU_DIR/"accessibility/verify-offload-playback.mjs"),
                    str(playback_manifest),str(evidence)],check=True,stdout=browser_log,stderr=subprocess.STDOUT,timeout=180,
                    env={**os.environ,"PEERTUBE_E2E_USER":peertube.ROOT_USER,"PEERTUBE_E2E_PASSWORD":peertube.ROOT_PASS})
            receipts["browser_playback"] = json.loads((evidence/"offload-playback.json").read_text())

        with step("actual unrenewed lease expires, denies late renewal and releases owned scratch"):
            processor = processors[0]
            response = processor.request("POST","/v1/jobs",json=_spec())
            assert response.status_code == 200
            job = response.json()["id"]
            assert processor.request("GET",f"/v1/jobs/{job}/artifacts/0",authorized=False).status_code == 401
            assert processor.health_or_none()["active_jobs"] == 1
            wait_for(lambda: processor.request("GET",f"/v1/jobs/{job}").status_code == 404,desc="real worker lease expires without renewal",timeout=40)
            assert processor.request("POST",f"/v1/jobs/{job}").status_code == 404
            wait_for(lambda: processor.health_or_none()["active_jobs"] == 0,desc="expired worker releases its reservation",timeout=10)
            assert not list((processor.root/"scratch/tmp").iterdir())
            receipts["real_lease_expiry"] = True

        with step("worker process crash preserves origin sources and processing succeeds on surviving lane"):
            crashed = processors[0]
            # VP9/Opus cannot take the H.264/AAC stream-copy fast path. Slow
            # encoding is an owned fixture setting, not a production default.
            slow = evidence/"forced-encode.webm"
            subprocess.run(["ffmpeg","-nostdin","-v","error","-f","lavfi","-i","testsrc2=size=1280x720:rate=15",
                "-f","lavfi","-i","sine=frequency=440:sample_rate=48000","-t","12","-c:v","libvpx-vp9",
                "-deadline","realtime","-cpu-used","8","-threads","2","-c:a","libopus","-y",str(slow)],
                check=True,capture_output=True,timeout=120)
            with psycopg.connect(origin.dsn,autocommit=True) as connection:
                connection.execute("UPDATE instance_settings SET media_video_preset='veryslow'")
            time.sleep(6)  # Normal settings cache expiry, not bypassed by test.
            third, _ = _submit(origin,users[0][0],channel,slow,f"Worker failure {label}")
            encoder = wait_for(lambda: _encoder(crashed),desc="worker A runs a real forced encode",timeout=30)
            assert origin.db_value("SELECT state='processing' FROM video_drafts WHERE media_id=%s",third)
            retained = _source_path(origin,third)
            assert _digest(retained) == _digest(slow)
            crashed.stop(crash=True)
            wait_for(lambda: _gone(encoder),desc="native encoder dies with its crashed parent",timeout=10)
            assert _digest(retained) == _digest(slow)
            _ready(origin,third)
            assert origin.db_value("SELECT source_bytes>0 AND state='ready' FROM video_drafts WHERE media_id=%s",third)
            assert processors[1].health_or_none()["completed_jobs"] >= 2
            crashed.launch()
            assert crashed.health_or_none()["active_jobs"] == 0
            assert not list((crashed.root/"scratch/tmp").iterdir())
            assert text.count("native video offload output verified") >= 2
            current_log = (Path(origin._dir.name)/"server.log").read_text()
            assert current_log.count("native video offload output verified") >= 3
            receipts["worker_crash_cleanup_and_survivor"] = {"real_encoder_interrupted":True,"source_sha256":_digest(slow),"draft_id":str(third)}

        with step("HTML cancellation fences publication and unrenewed remote encoding is retired"):
            survivor = processors[1]
            completed = survivor.health_or_none()["completed_jobs"]
            cancellation_source = evidence/"cancellation-source.webm"
            subprocess.run(["ffmpeg","-nostdin","-v","error","-stream_loop","4","-i",str(slow),
                "-c","copy","-y",str(cancellation_source)],check=True,capture_output=True,timeout=30)
            cancelled, session = _submit(origin,users[1][0],second_channel,cancellation_source,f"Cancelled encoding {label}")
            encoder = wait_for(lambda: _encoder(survivor),desc="worker B runs the cancellable encode",timeout=30)
            draft = session.get(f"{origin.url}/videos/drafts/{cancelled}",timeout=30)
            cancelled_at = time.monotonic()
            response = session.post(f"{origin.url}/web/videos/drafts/{cancelled}",
                data={"csrf":_csrf(draft.text),"op":"cancel","confirm":"yes"},allow_redirects=False,timeout=30)
            assert response.status_code == 303
            wait_for(lambda: survivor.health_or_none()["active_jobs"] == 0,desc="ordinary cancelled claim sends bounded processor DELETE",timeout=10)
            cancellation_seconds = time.monotonic()-cancelled_at
            wait_for(lambda: _gone(encoder),desc="cancelled remote encoder exits",timeout=10)
            assert survivor.health_or_none()["completed_jobs"] == completed
            assert not list((survivor.root/"scratch/tmp").iterdir())
            assert not origin.db_value("SELECT EXISTS(SELECT 1 FROM video_publications WHERE media_id=%s)",cancelled)
            assert requests.get(f"{origin.url}/media/hls/{cancelled}/master.m3u8",verify=False,timeout=30).status_code == 404
            receipts["html_cancellation"] = {"draft_id":str(cancelled),"no_publication":True,"remote_encoder_retired":True,"seconds":cancellation_seconds}

        receipts["candidate_sha256"] = candidate
        (evidence/"result.json").write_text(json.dumps(receipts,indent=2))
        print(f"Protected native processor evidence: {evidence}")
    finally:
        if origin._dir is not None and (Path(origin._dir.name)/"server.log").exists():
            (evidence/"origin.log").write_text((Path(origin._dir.name)/"server.log").read_text())
        try:
            for processor in processors:
                processor.stop()
            if pt is not None:
                for item in followed:
                    pt.delete(f"/api/v1/users/me/subscriptions/{item['username']}@{origin.domain}")
            for (_,api),item in zip(users,[channel,second_channel]):
                if item is not None:
                    api.post(f"/api/v1/video_channels/{item['id']}",operation="delete",confirm=item['username'])
        finally:
            origin.stop()
            for path in evidence.rglob("*"):
                if path.is_file(): path.chmod(0o700 if path.name == "candidate-plamenu" else 0o600)
