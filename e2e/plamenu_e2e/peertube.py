"""Helpers for the disposable PeerTube peer at peertube.local.

PeerTube speaks its own REST API (not Mastodon's) and federates *video* —
each video is an ActivityPub `Video` object whose `url` array carries an
`application/x-mpegURL` HLS master playlist Link. This is a standalone client
like lemmy.py / sharkey.py: every endpoint takes and returns JSON.

The external harness uses the official
`chocobozzz/peertube:production-bookworm` image and configures it to trust the
local Caddy CA. See e2e/README.md for the public harness status.

The standing admin is `root` (PeerTube fixes the username); its password is
seeded by the harness. A bearer token is minted via the OAuth password grant.
"""

import contextlib
import subprocess
import tempfile
import time
from pathlib import Path

import requests
import urllib3

from . import config

urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

ROOT_USER = "root"
ROOT_PASS = "peertube-root-pass-123"
TOKEN_FILE = config.PEERTUBE_DIR / ".root-token"

# PeerTube's VideoState enum (packages/models/src/videos/video-state.enum.ts).
STATE_PUBLISHED = 1
STATE_WAITING_FOR_LIVE = 4
STATE_LIVE_ENDED = 5


class PeertubeError(RuntimeError):
    pass


class PeertubeApi:
    def __init__(self, base_url: str, token: str | None = None):
        self.base_url = base_url.rstrip("/")
        self.http = requests.Session()
        self.http.verify = False
        if token:
            self.http.headers["Authorization"] = f"Bearer {token}"

    def _request(self, method: str, path: str, *, params=None, **kw):
        r = self.http.request(
            method, self.base_url + path, params=params, timeout=120, **kw
        )
        if not r.ok:
            raise PeertubeError(f"{method} {path} -> {r.status_code}: {r.text[:500]}")
        if not r.content:
            return None
        return r.json()

    def get(self, path: str, **params):
        return self._request("GET", path, params=params)

    def post(self, path: str, **json):
        return self._request("POST", path, json=json)

    def put(self, path: str, **json):
        return self._request("PUT", path, json=json)

    def delete(self, path: str):
        return self._request("DELETE", path)

    # ── domain helpers ────────────────────────────────────────────────

    def config_(self) -> dict:
        """Public instance config (unauthenticated; the reachability probe)."""
        return self.get("/api/v1/config")

    def me(self) -> dict:
        return self.get("/api/v1/users/me")

    def default_channel_id(self) -> int:
        """root's first (auto-created) video channel id — the upload target."""
        channels = self.me()["videoChannels"]
        if not channels:
            raise PeertubeError("account has no video channel")
        return channels[0]["id"]

    def video(self, id_or_uuid) -> dict:
        """Full video detail (includes `streamingPlaylists`, `state`, `url`)."""
        return self.get(f"/api/v1/videos/{id_or_uuid}")

    def videos(self, **params) -> dict:
        """List videos; `{total, data}`. Pass isLocal='true' for local ones."""
        return self.get("/api/v1/videos", **params)

    def upload_video(
        self,
        path,
        name: str,
        *,
        privacy: int = 1,
        channel_id: int | None = None,
    ) -> dict:
        """Legacy multipart upload (POST /api/v1/videos/upload); returns the
        created `{id, uuid, shortUUID}`.

        privacy 1 = Public, 2 = Unlisted, 3 = Private, 4 = Internal.
        """
        if channel_id is None:
            channel_id = self.default_channel_id()
        with open(path, "rb") as fh:
            files = {"videofile": (Path(path).name, fh, "video/mp4")}
            data = {
                "name": name,
                "channelId": str(channel_id),
                "privacy": str(privacy),
            }
            r = self.http.post(
                self.base_url + "/api/v1/videos/upload",
                data=data,
                files=files,
                timeout=180,
            )
        if not r.ok:
            raise PeertubeError(f"upload -> {r.status_code}: {r.text[:500]}")
        return r.json()["video"]

    def wait_for_hls(
        self, id_or_uuid, *, timeout: float = 180.0, interval: float = 3.0
    ) -> dict:
        """Poll until the video has ≥1 streaming playlist (HLS transcoded).

        Returns the final video detail. PeerTube runs HLS transcoding as a
        background job, so a freshly uploaded video has an empty
        `streamingPlaylists` for a few seconds.
        """
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            last = self.video(id_or_uuid)
            if last.get("streamingPlaylists"):
                return last
            time.sleep(interval)
        state = (last or {}).get("state")
        raise PeertubeError(
            f"HLS not ready for {id_or_uuid} within {timeout}s (state={state})"
        )

    # ── live ──────────────────────────────────────────────────────────
    #
    # A PeerTube live is a Video that exists before it has any media: it is
    # created in state WAITING_FOR_LIVE with an empty `url` ladder, gains an
    # `application/x-mpegURL` master (and state PUBLISHED) when RTMP starts
    # flowing, and drops back to LIVE_ENDED (or WAITING_FOR_LIVE, when
    # permanent) when the broadcast stops. Each transition federates an
    # Update. Unlike VOD, the master's `tag` array carries NO file Links —
    # the rendition ladder only exists inside the playlist itself.

    def create_live(
        self,
        name: str,
        *,
        privacy: int = 1,
        channel_id: int | None = None,
        permanent: bool = False,
        save_replay: bool = False,
        latency_mode: int = 1,
    ) -> dict:
        """Create a live video; returns `{id, uuid, shortUUID}`.

        `latency_mode` 1 = default, 2 = small latency, 3 = high latency.
        """
        if channel_id is None:
            channel_id = self.default_channel_id()
        body = {
            "channelId": channel_id,
            "name": name,
            "privacy": privacy,
            "permanentLive": permanent,
            "saveReplay": save_replay,
            "latencyMode": latency_mode,
        }
        if save_replay:
            body["replaySettings"] = {"privacy": privacy}
        return self._request("POST", "/api/v1/videos/live", json=body)["video"]

    def delete_video(self, id_or_uuid) -> None:
        """Remove a video (best effort). A live that is merely *announced*
        counts against the per-user live limit until it is deleted, so tests
        that create lives should clean them up."""
        try:
            self.delete(f"/api/v1/videos/{id_or_uuid}")
        except PeertubeError:
            pass

    def live_config(self, video_id) -> dict:
        """Live settings incl. `streamKey` and `rtmpUrl` (owner-only)."""
        return self.get(f"/api/v1/videos/live/{video_id}")

    def rtmp_target(self, video_id) -> str:
        """The full `rtmp://…/live/<streamKey>` URL an encoder pushes to."""
        live = self.live_config(video_id)
        return f"{live['rtmpUrl'].rstrip('/')}/{live['streamKey']}"

    def wait_for_state(
        self, id_or_uuid, state: int, *, timeout: float = 120.0, interval: float = 2.0
    ) -> dict:
        """Poll until the video reaches `state`; returns the final detail."""
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            last = self.video(id_or_uuid)
            if last["state"]["id"] == state:
                return last
            time.sleep(interval)
        raise PeertubeError(
            f"{id_or_uuid} did not reach state {state} within {timeout}s "
            f"(last={(last or {}).get('state')})"
        )

    def set_live_transcoding(self, enabled: bool, resolutions: list[int] = ()) -> None:
        """Toggle live transcoding instance-wide (a rendition ladder +, on 8.x,
        a separated audio track instead of one muxed variant).

        Applied at runtime — PeerTube re-reads it without a restart.
        """
        custom = self.get("/api/v1/config/custom")
        custom["live"]["transcoding"]["enabled"] = enabled
        if resolutions:
            for key in custom["live"]["transcoding"]["resolutions"]:
                height = int(key.rstrip("p"))
                custom["live"]["transcoding"]["resolutions"][key] = (
                    height in resolutions
                )
        self.put("/api/v1/config/custom", **custom)

    def ap_id(self, uuid: str) -> str:
        """The video's canonical ActivityPub id (its `id`/`url` for AS2)."""
        return f"{self.base_url}/videos/watch/{uuid}"

    def fetch_ap(self, uuid: str) -> dict:
        """Dereference the video as ActivityPub JSON (type: Video)."""
        r = self.http.get(
            self.ap_id(uuid),
            headers={"Accept": "application/activity+json"},
            timeout=30,
        )
        if not r.ok:
            raise PeertubeError(f"AP fetch {uuid} -> {r.status_code}: {r.text[:300]}")
        return r.json()

    def hls_playlist_url(self, uuid: str) -> str | None:
        """The `application/x-mpegURL` master-playlist href from the AP object,
        or None. This is what a federated consumer plays."""
        for entry in self.fetch_ap(uuid).get("url", []):
            if entry.get("mediaType") == "application/x-mpegURL":
                return entry.get("href")
        return None


def reachable() -> bool:
    """Whether the PeerTube test instance answers (tests skip when it doesn't)."""
    try:
        PeertubeApi(config.PEERTUBE_URL).config_()
        return True
    except (PeertubeError, requests.RequestException):
        return False


def mint_token(user: str = ROOT_USER, password: str = ROOT_PASS) -> str:
    """OAuth password grant: fetch the local client, exchange root creds."""
    api = PeertubeApi(config.PEERTUBE_URL)
    oc = api.get("/api/v1/oauth-clients/local")
    r = api.http.post(
        api.base_url + "/api/v1/users/token",
        data={
            "client_id": oc["client_id"],
            "client_secret": oc["client_secret"],
            "grant_type": "password",
            "username": user,
            "password": password,
        },
        timeout=30,
    )
    if not r.ok:
        raise PeertubeError(f"/users/token -> {r.status_code}: {r.text[:300]}")
    return r.json()["access_token"]


def root() -> PeertubeApi:
    """Authenticated client for the standing admin, token cached on disk."""
    if TOKEN_FILE.is_file():
        api = PeertubeApi(config.PEERTUBE_URL, token=TOKEN_FILE.read_text().strip())
        try:
            api.me()  # 401 when the token is stale
            return api
        except PeertubeError:
            pass
    token = mint_token()
    TOKEN_FILE.write_text(f"{token}\n")
    return PeertubeApi(config.PEERTUBE_URL, token=token)


def make_sample_mp4(path, seconds: int = 5) -> Path:
    """Generate a tiny H.264/AAC test clip with the host ffmpeg."""
    path = Path(path)
    subprocess.run(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            f"testsrc=duration={seconds}:size=320x240:rate=30",
            "-f",
            "lavfi",
            "-i",
            f"sine=frequency=440:duration={seconds}",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            "-y",
            str(path),
        ],
        check=True,
    )
    return path


@contextlib.contextmanager
def broadcast(
    api: "PeertubeApi",
    video_id,
    *,
    size: str = "640x360",
    rate: int = 30,
    wait_published: bool = True,
    timeout: float = 180.0,
):
    """Push a synthetic RTMP stream into a live video for the duration of the
    block, and stop it on the way out.

    The RTMP port is published to the host by peertube-test/compose.dev.yml,
    so the host ffmpeg can reach `rtmp://peertube.local:1935/live/<key>`
    directly. PeerTube only flips the video to PUBLISHED (and federates the
    go-live Update) a few segments into the stream, so by default this waits
    for that transition before yielding.
    """
    target = api.rtmp_target(video_id)
    proc = subprocess.Popen(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            f"testsrc2=size={size}:rate={rate}",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-tune",
            "zerolatency",
            "-g",
            str(rate * 2),
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-f",
            "flv",
            target,
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    try:
        if wait_published:
            try:
                api.wait_for_state(video_id, STATE_PUBLISHED, timeout=timeout)
            except PeertubeError:
                if proc.poll() is not None:
                    err = (proc.stderr.read() or b"").decode(errors="replace")
                    raise PeertubeError(f"ffmpeg RTMP push died: {err[:500]}") from None
                raise
        yield proc
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=10)


def ensure_test_video(name: str = "Plamenu e2e test video") -> dict:
    """Idempotently guarantee a public, HLS-transcoded local video exists;
    returns its full detail. Reuses the newest existing local video if any."""
    api = root()
    existing = api.videos(count=1, isLocal="true", sort="-publishedAt")
    if existing.get("total", 0) >= 1:
        return api.video(existing["data"][0]["uuid"])
    with tempfile.TemporaryDirectory() as d:
        sample = make_sample_mp4(Path(d) / "sample.mp4", seconds=5)
        created = api.upload_video(sample, name)
    return api.wait_for_hls(created["uuid"])


if __name__ == "__main__":  # `python -m plamenu_e2e.peertube` seeds a video
    v = ensure_test_video()
    print(f"uuid={v['uuid']} state={v.get('state')}")
    print(f"AP id: {root().ap_id(v['uuid'])}")
