"""Maximum-default-size transfers of a decodable one-hour VOD, not RAM blobs.

ISO-BMFF free padding isolates upload-byte stress from encoder complexity.
The separate real 1080p ladder and difficult-input tests cover that complexity.
"""
import hashlib
import io
import json
import re
import struct
import subprocess
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest
import psycopg
import requests
from plamenu_e2e import config, unique
from plamenu_e2e.steps import step, wait_for

MAX_SOURCE = 2 * 1024**3
CHUNK = 8 * 1024**2


def csrf(html):
    return re.search(r'name="csrf" value="([^"]+)"', html)[1]


class MultipartFile:
    """Known Content-Length with at most 64 KiB reads; requests.files buffers."""
    def __init__(self, path, fields):
        self.boundary = unique("boundary")
        prefix = b"".join(
            f'--{self.boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode()
            for name, value in fields.items()
        ) + f'--{self.boundary}\r\nContent-Disposition: form-data; name="file"; filename="hour.mp4"\r\nContent-Type: video/mp4\r\n\r\n'.encode()
        suffix = f'\r\n--{self.boundary}--\r\n'.encode()
        self.file = path.open("rb")
        self.parts = [io.BytesIO(prefix), self.file, io.BytesIO(suffix)]
        self.total = len(prefix) + path.stat().st_size + len(suffix)
        self.position = 0

    def __len__(self):
        return self.total

    def tell(self):
        return self.position

    def read(self, amount=-1):
        amount = min(amount if amount > 0 else 65536, 65536)
        output = bytearray()
        while self.parts and len(output) < amount:
            data = self.parts[0].read(amount - len(output))
            output.extend(data)
            if not data:
                self.parts.pop(0)
        self.position += len(output)
        return bytes(output)

    def close(self):
        self.file.close()


class Resources:
    """Sample only the disposable instance and its own encoder children."""
    def __init__(self, instance):
        self.instance = instance
        self.parent_peak_kib = self.child_peak_kib = self.disk_peak = 0
        self.jobs_peak = self.uploaders_peak = 0
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.sample, daemon=True)

    def sample(self):
        with psycopg.connect(self.instance.dsn, autocommit=True) as connection:
            self.sample_process(connection)

    def sample_process(self, connection):
        while not self.stop.is_set():
            try:
                process = self.instance._proc.pid
                status = Path(f"/proc/{process}/status").read_text()
                rss = int(re.search(r"VmRSS:\s+(\d+)", status)[1])
                self.parent_peak_kib = max(self.parent_peak_kib, rss)
                # Tokio may fork from any worker thread; the leader's children
                # file alone would falsely report zero encoder memory.
                children = set()
                for task in Path(f"/proc/{process}/task").glob("*/children"):
                    try:
                        children.update(task.read_text().split())
                    except FileNotFoundError:
                        pass
                child_rss = 0
                for child in children:
                    try:
                        value = re.search(r"VmRSS:\s+(\d+)", Path(f"/proc/{child}/status").read_text())
                        child_rss += int(value[1]) if value else 0
                    except FileNotFoundError:
                        pass
                self.child_peak_kib = max(self.child_peak_kib, child_rss)
                root = Path(self.instance._dir.name) / "media"
                self.disk_peak = max(self.disk_peak, sum(p.stat().st_size for p in root.rglob("*") if p.is_file()))
                jobs, uploaders = connection.execute("SELECT count(*),count(DISTINCT account_id) FROM video_drafts WHERE state='processing' AND lease_until>now()").fetchone()
                self.jobs_peak = max(self.jobs_peak, jobs)
                self.uploaders_peak = max(self.uploaders_peak, uploaders)
            except (FileNotFoundError, TypeError):
                pass  # A deliberately killed process/renamed scratch is transient.
            self.stop.wait(.05)

    def __enter__(self):
        self.sample_once = int(re.search(r"VmRSS:\s+(\d+)", Path(f"/proc/{self.instance._proc.pid}/status").read_text())[1])
        self.started = time.monotonic()
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.stop.set()
        self.thread.join(timeout=3)


def hour_source(path):
    subprocess.run([
        "ffmpeg", "-nostdin", "-v", "error", "-f", "lavfi", "-i", "color=c=blue:size=320x240:rate=1",
        "-t", "3600", "-c:v", "libx264", "-preset", "ultrafast", "-g", "4", "-pix_fmt", "yuv420p",
        "-movflags", "+faststart", str(path),
    ], check=True, timeout=60)
    original = path.stat().st_size
    with path.open("ab") as source:
        source.write(struct.pack(">I4s", MAX_SOURCE - original, b"free"))
        source.truncate(MAX_SOURCE)
    assert path.stat().st_size == MAX_SOURCE


@pytest.mark.parametrize("mode", ["multipart", "resumable"])
def test_default_maximum_decodable_hour_survives_upload_and_processing(plamenu2, plamenu2_user, tmp_path, mode):
    api = plamenu2.login(plamenu2_user)
    channel = api.post("/api/v1/video_channels", username=unique("hour"), display_name="Long-form films")
    browser = requests.Session()
    browser.verify = False
    assert browser.post(f"{plamenu2.url}/login", data={"email": plamenu2_user.email, "password": plamenu2_user.password},
                        allow_redirects=False, timeout=30).status_code == 303
    token = csrf(browser.get(f"{plamenu2.url}/videos", timeout=30).text)
    source = tmp_path / "hour.mp4"
    hour_source(source)
    fields = {"csrf": token, "channel": channel["id"], "title": "One full hour", "visibility": "unlisted",
              "language": "en", "alt": "A steady blue image", "transcript": "An intentionally silent blue image for one hour."}
    draft_id = None
    with Resources(plamenu2) as resources:
        with step("stream an actual default-ceiling source without allocating its body"):
            if mode == "multipart":
                body = MultipartFile(source, fields)
                try:
                    response = browser.post(f"{plamenu2.url}/web/videos", data=body,
                        headers={"Content-Type": f"multipart/form-data; boundary={body.boundary}"},
                        allow_redirects=False, timeout=240)
                finally:
                    body.close()
                assert response.status_code == 303, response.text[:500]
                draft_id = response.headers["Location"].rsplit("/", 1)[1]
            else:
                with source.open("rb") as file:
                    first = file.read(65536)
                    file.seek(-65536, 2)
                    fingerprint = hashlib.sha256(first + file.read(65536)).hexdigest()
                response = browser.post(f"{plamenu2.url}/web/videos/resumable",
                    files=[(key, (None, str(value))) for key, value in {**fields,
                           "source_size": MAX_SOURCE, "source_fingerprint": fingerprint}.items()], timeout=30)
                assert response.status_code == 200, response.text[:500]
                draft_id = response.json()["id"]
                route = f"{plamenu2.url}/web/videos/drafts/{draft_id}/source"
                with source.open("rb") as file:
                    for offset in range(0, MAX_SOURCE, CHUNK):
                        data = file.read(CHUNK)
                        response = browser.patch(route, data=data,
                            headers={"x-csrf-token": token, "x-upload-offset": str(offset)}, timeout=60)
                        assert response.status_code == 200, response.text[:500]
                        assert response.json()["offset"] == offset + len(data)
                        if offset == 0:
                            plamenu2.restart_process_after_crash()
                            persisted = browser.get(route, timeout=30).json()
                            assert persisted["offset"] == CHUNK and persisted["fingerprint"] == fingerprint
                assert browser.post(f"{route}/finish", headers={"x-csrf-token": token}, timeout=30).status_code == 200
            resources.transfer_seconds = time.monotonic() - resources.started
        with step("the real worker retains all 3600 seconds, privately and durably"):
            def ready():
                response = browser.get(f"{plamenu2.url}/videos/drafts/{draft_id}/status", timeout=30)
                response.raise_for_status()
                assert response.json()["phase"] != "failed", plamenu2.log_tail()
                return response.json()["phase"] == "ready"
            wait_for(ready, timeout=240, desc="one-hour source finishes processing")
            row = plamenu2.db.conn.execute("SELECT d.source_bytes,m.duration,m.file_size,m.file_name FROM video_drafts d JOIN media_attachments m ON m.id=d.media_id WHERE d.media_id=%s", (draft_id,)).fetchone()
            assert row[0] == MAX_SOURCE and abs(row[1] - 3600) < .1
            assert row[2] < 32 * 1024**2, "ISO free padding must not pollute the playable rendition"
            assert browser.get(f"{plamenu2.url}/media/{row[3]}", timeout=30).status_code == 404
            rendition = tmp_path / "processed.mp4"
            response = browser.get(f"{plamenu2.url}/videos/drafts/{draft_id}/preview/video", timeout=60)
            response.raise_for_status()
            rendition.write_bytes(response.content)
            probe = json.loads(subprocess.check_output(["ffprobe", "-v", "error", "-show_streams", "-show_format", "-of", "json", str(rendition)]))
            assert abs(float(probe["format"]["duration"]) - 3600) < .1
            assert probe["streams"][0]["codec_name"] == "h264"
            assert int(probe["streams"][0]["nb_frames"]) == 3600
            plamenu2.restart_process_after_crash()
            assert ready(), "a committed ready generation must survive a process crash"
            assert resources.parent_peak_kib - resources.sample_once < 128 * 1024
            assert 0 < resources.child_peak_kib < 256 * 1024
    report = {"mode": mode, "source_bytes": MAX_SOURCE, "duration_seconds": 3600, "input": "320x240/1fps silent H.264 plus sparse ISO free padding",
              "transfer_seconds": resources.transfer_seconds, "total_seconds": time.monotonic() - resources.started,
              "parent_baseline_kib": resources.sample_once, "parent_peak_kib": resources.parent_peak_kib,
              "encoder_children_peak_kib": resources.child_peak_kib, "media_logical_peak_bytes": resources.disk_peak}
    evidence = config.PLAMENU_DIR / f"target/e2e/ap09-long-form-{mode}.json"
    evidence.parent.mkdir(parents=True, exist_ok=True)
    evidence.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report))
    with step("above-ceiling admission rejects before transferring any source bytes"):
        response = browser.post(f"{plamenu2.url}/web/videos/resumable",
            files=[(key, (None, str(value))) for key, value in {**fields,
                   "source_size": MAX_SOURCE + 1, "source_fingerprint": "a" * 64}.items()], timeout=30)
        assert response.status_code == 413, response.text[:500]
    with step("confirmed cancellation reclaims journaled files and acknowledged quota"):
        review = browser.get(f"{plamenu2.url}/videos/drafts/{draft_id}", timeout=30)
        response = browser.post(f"{plamenu2.url}/web/videos/drafts/{draft_id}",
            data={"csrf": csrf(review.text), "op": "cancel", "confirm": "yes"}, allow_redirects=False, timeout=30)
        assert response.status_code == 303, response.text[:500]
        wait_for(lambda: plamenu2.db_value("SELECT count(*) FROM video_storage_reservations WHERE media_id=%s", draft_id) == 0,
                 timeout=90, desc="acknowledged deletion refunds all original and output bytes")


def test_concurrent_real_uploads_use_two_bounded_worker_slots_and_two_uploaders(plamenu2, plamenu2_user, tmp_path):
    users = [plamenu2_user, plamenu2.account("parallel")]
    sessions = []
    for user in users:
        api = plamenu2.login(user)
        channel = api.post("/api/v1/video_channels", username=unique("parallel"), display_name="Parallel films")
        browser = requests.Session()
        browser.verify = False
        assert browser.post(f"{plamenu2.url}/login", data={"email":user.email,"password":user.password}, allow_redirects=False, timeout=30).status_code == 303
        token = csrf(browser.get(f"{plamenu2.url}/videos",timeout=30).text)
        sessions.append((browser,{"csrf":token,"channel":channel["id"],"title":"Concurrent film","visibility":"unlisted"}))
    # MPEG-4 Part 2 forces the native H.264 encoder; padding stresses transfer
    # without claiming this synthetic workload represents production traffic.
    source = tmp_path / "parallel.mp4"
    subprocess.run(["ffmpeg","-nostdin","-v","error","-f","lavfi","-i","testsrc2=size=1280x720:rate=15",
        "-t","30","-c:v","mpeg4","-q:v","3","-movflags","+faststart",str(source)],check=True,timeout=60)
    ceiling = 128 * 1024**2
    with source.open("ab") as file:
        file.write(struct.pack(">I4s",ceiling-source.stat().st_size,b"free"))
        file.truncate(ceiling)
    barrier = threading.Barrier(3)
    def upload(owner):
        browser, fields = sessions[owner]
        # Requests sessions are not shared by concurrent writers.
        writer = requests.Session()
        writer.verify = False
        writer.cookies.update(browser.cookies)
        body = MultipartFile(source,fields)
        try:
            barrier.wait(timeout=30)
            response = writer.post(f"{plamenu2.url}/web/videos",data=body,
                headers={"Content-Type":f"multipart/form-data; boundary={body.boundary}"},allow_redirects=False,timeout=180)
            assert response.status_code == 303,response.text[:500]
            return owner,response.headers["Location"].rsplit("/",1)[1]
        finally:
            body.close()
    with Resources(plamenu2) as resources:
        with ThreadPoolExecutor(max_workers=3) as writers:
            futures = [writers.submit(upload,owner) for owner in [0,0,1]]
            drafts = [future.result(timeout=240) for future in futures]
        def ready():
            phases = [sessions[owner][0].get(f"{plamenu2.url}/videos/drafts/{media}/status",timeout=30).json()["phase"] for owner,media in drafts]
            assert "failed" not in phases,plamenu2.log_tail()
            return all(phase=="ready" for phase in phases)
        wait_for(ready,timeout=240,desc="all three concurrent re-encoded uploads finish")
    report = {"source_bytes_each":ceiling,"inputs":3,"accounts":2,"duration_seconds_each":30,
        "fixture":"1280x720/15fps MPEG-4 test pattern plus ISO free padding",
        "total_seconds":time.monotonic()-resources.started,"worker_jobs_peak":resources.jobs_peak,
        "active_uploaders_peak":resources.uploaders_peak,"parent_baseline_kib":resources.sample_once,
        "parent_peak_kib":resources.parent_peak_kib,"encoder_children_peak_kib":resources.child_peak_kib,
        "media_logical_peak_bytes":resources.disk_peak}
    evidence = config.PLAMENU_DIR / "target/e2e/ap09-concurrent-real-upload.json"
    evidence.write_text(json.dumps(report,indent=2)+"\n")
    print(json.dumps(report))
    assert resources.jobs_peak == 2 and resources.uploaders_peak == 2,report
    assert resources.parent_peak_kib-resources.sample_once < 128*1024,report
    assert 0 < resources.child_peak_kib < 768*1024,report
    for owner,media in drafts:
        assert abs(plamenu2.db_value("SELECT duration FROM media_attachments WHERE id=%s",media)-30)<.1
        browser,fields = sessions[owner]
        assert browser.post(f"{plamenu2.url}/web/videos/drafts/{media}",data={"csrf":fields["csrf"],"op":"cancel","confirm":"yes"},allow_redirects=False,timeout=30).status_code == 303
    wait_for(lambda: all(plamenu2.db_value("SELECT count(*) FROM video_storage_reservations WHERE media_id=%s",media)==0 for _,media in drafts),timeout=90,desc="all concurrent output reservations refunded after deletion")
