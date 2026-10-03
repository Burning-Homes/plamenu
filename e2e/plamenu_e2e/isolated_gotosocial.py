"""Guarded temporary local route to a new owned official GoToSocial volume.

Original proxy source/active configuration and standing peer are preserved.
Only the new owned server is stopped; bootstrap containers and data retained.
"""

import copy
import hashlib
import json
import secrets
import subprocess
import time
from pathlib import Path

IMAGE = "docker.io/superseriousbusiness/gotosocial:0.22.1@sha256:0078ca451dda5e8c4a8630784519f99b69f48c3dda79586a9703d4bf8ea14dae"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def fingerprint(v):
    return hashlib.sha256(
        json.dumps(v, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()


def isolated_route(configuration, dial):
    """Change only the known GoToSocial upstream; reject ambiguous ownership."""
    replacement = copy.deepcopy(configuration)
    routes = replacement["apps"]["http"]["servers"]["srv0"]["routes"]
    matches = [r for r in routes if r.get("match") == [{"host": ["gotosocial.local"]}]]
    require(len(matches) == 1, "Expected one exact GoToSocial proxy route")
    upstreams = matches[0]["handle"][0]["routes"][0]["handle"][0]["upstreams"]
    require(
        upstreams == [{"dial": "gotosocial-web:8080"}],
        "Standing proxy upstream differs",
    )
    upstreams[0]["dial"] = dial
    return replacement


class IsolatedGoToSocial:
    def __init__(self, state, environment, output, *, peer_root, proxy, standing):
        self.fleet = Path(peer_root).resolve()
        self.proxy = proxy
        self.standing = standing
        self.root = Path(state) / "gotosocial-owned"
        self.root.mkdir(mode=0o700)
        self.env = environment
        self.output = output
        self.nonce = secrets.token_hex(12)
        self.label = "plamenu.validation.invocation=" + self.nonce
        self.peer_id = None
        self.route_attempted = False
        self.proxy_before = None
        self.standing_before = None
        self.original = None
        self.replacement = None
        self.receipt = {
            "existing_credentials_changed": False,
            "standing_volume_mounted": False,
            "http_signature_policy_changed": False,
            "image": IMAGE,
            "data_preserved": True,
        }

    def command(self, args, *, capture=False, input=None):
        r = subprocess.run(
            args,
            check=False,
            env=self.env,
            text=True,
            input=input,
            stdout=subprocess.PIPE if capture else self.output,
            stderr=self.output,
        )
        require(
            r.returncode == 0,
            "Isolated peer command failed; inspect private fixture log",
        )
        return r.stdout.strip() if capture else None

    def inspect(self, name):
        return json.loads(self.command(["docker", "inspect", name], capture=True))[0]

    def active(self):
        return json.loads(
            self.command(
                [
                    "docker",
                    "exec",
                    self.proxy,
                    "wget",
                    "-qO-",
                    "http://127.0.0.1:2019/config/",
                ],
                capture=True,
            )
        )

    def reload(self, value):
        self.command(
            ["docker", "exec", "-i", self.proxy, "caddy", "reload", "--config", "-"],
            input=json.dumps(value),
        )
        require(
            self.active() == value,
            "Local proxy active configuration differs after reload",
        )

    def snapshot(self, container):
        info = self.inspect(container)
        return {
            "id": info["Id"],
            "image": info["Image"],
            "pid": info["State"]["Pid"],
            "started_at": info["State"]["StartedAt"],
            "restart_count": info["RestartCount"],
            "running": info["State"]["Running"],
        }

    def save(self):
        (self.root / "isolation-status.json").write_text(
            json.dumps(self.receipt, indent=2) + "\n"
        )

    def start(self):
        require(
            self.env.get("GOTOSOCIAL_URL", "https://gotosocial.local")
            == "https://gotosocial.local",
            "Isolated fixture requires the canonical local GoToSocial URL",
        )
        self.command(["docker", "image", "inspect", IMAGE])
        proxy = self.inspect(self.proxy)
        sources = [
            m["Source"]
            for m in proxy["Mounts"]
            if m["Type"] == "bind" and m["Destination"] == "/etc/caddy/Caddyfile"
        ]
        require(len(sources) == 1, "Expected one mounted local test Caddyfile")
        self.source = Path(sources[0])
        self.before_source = hashlib.sha256(self.source.read_bytes()).hexdigest()
        self.original = self.active()
        (self.root / "proxy-before.private.json").write_text(
            json.dumps(self.original, indent=2) + "\n"
        )
        self.proxy_before = self.snapshot(self.proxy)
        self.standing_before = self.snapshot(self.standing)
        self.standing_sources = {
            m["Source"] for m in self.inspect(self.standing)["Mounts"] if m["RW"]
        }
        config = (self.fleet / "gotosocial-test/config.yaml").read_text()
        (self.root / "config.yaml").write_text(config)
        # New account only, using the canonical disposable-fixture identity.
        # No existing account/password/admin grant is touched.
        (self.root / "bootstrap.sh").write_text(
            'set -eu\n/gotosocial/gotosocial --config-path /fixture/config.yaml admin account create --username dave --email dave@gotosocial.local --password "$(cat /fixture/password.private)"\n/gotosocial/gotosocial --config-path /fixture/config.yaml admin account confirm --username dave\n/gotosocial/gotosocial --config-path /fixture/config.yaml admin account promote --username dave\n'
        )
        (self.root / "password.private").write_text(secrets.token_urlsafe(32))
        (self.root / "password.private").chmod(0o600)
        ca = self.fleet / "mastodon-test/ca-bundle.crt"
        volume = "plamenu-gts-isolation-" + self.nonce
        mounts = [
            "--volume",
            str(self.root) + ":/fixture:ro",
            "--volume",
            str(self.root / "config.yaml") + ":/gotosocial/config.yaml:ro",
            "--volume",
            str(ca) + ":/etc/ssl/certs/ca-certificates.crt:ro",
            "--volume",
            volume + ":/gotosocial/storage",
        ]
        self.peer_id = self.command(
            [
                "docker",
                "run",
                "--pull=never",
                "--detach",
                "--name",
                "plamenu-gts-isolated-" + self.nonce,
                "--network",
                "mastodon-test_internal",
                "--cpus",
                "2",
                "--memory",
                "512m",
                "--label",
                self.label,
                *mounts,
                IMAGE,
                "--config-path",
                "/gotosocial/config.yaml",
            ],
            capture=True,
        )
        require(len(self.peer_id) == 64, "New peer container identity missing")
        deadline = time.monotonic() + 120
        while subprocess.run(
            [
                "docker",
                "exec",
                self.peer_id,
                "wget",
                "-q",
                "--spider",
                "http://127.0.0.1:8080/nodeinfo/2.0",
            ],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode:
            require(
                time.monotonic() < deadline
                and self.inspect(self.peer_id)["State"]["Running"],
                "New peer readiness failed",
            )
            time.sleep(1)
        # GoToSocial initializes its instance OAuth application on first serve.
        # Stop only this exact new fixture before CLI account provisioning.
        info = self.inspect(self.peer_id)
        require(
            info["Config"]["Labels"].get("plamenu.validation.invocation") == self.nonce,
            "New peer ownership differs before seed",
        )
        self.command(["docker", "stop", "--time", "20", self.peer_id])
        self.command(
            [
                "docker",
                "run",
                "--pull=never",
                "--name",
                "plamenu-gts-bootstrap-" + self.nonce,
                "--network",
                "none",
                "--cpus",
                "2",
                "--memory",
                "512m",
                "--label",
                self.label,
                *mounts,
                "--entrypoint",
                "/bin/sh",
                IMAGE,
                "/fixture/bootstrap.sh",
            ]
        )
        self.command(["docker", "start", self.peer_id])
        deadline = time.monotonic() + 120
        while subprocess.run(
            [
                "docker",
                "exec",
                self.peer_id,
                "wget",
                "-q",
                "--spider",
                "http://127.0.0.1:8080/nodeinfo/2.0",
            ],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode:
            require(
                time.monotonic() < deadline
                and self.inspect(self.peer_id)["State"]["Running"],
                "Seeded peer readiness failed",
            )
            time.sleep(1)
        peer = self.inspect(self.peer_id)
        require(
            peer["Config"]["Labels"].get("plamenu.validation.invocation") == self.nonce,
            "New peer ownership differs",
        )
        require(
            all(m.get("Source") not in self.standing_sources for m in peer["Mounts"]),
            "Standing peer volume unexpectedly mounted",
        )
        ip = peer["NetworkSettings"]["Networks"]["mastodon-test_internal"]["IPAddress"]
        require(ip.startswith("172."), "Unexpected local peer network address")
        self.replacement = isolated_route(self.original, ip + ":8080")
        require(
            self.active() == self.original
            and hashlib.sha256(self.source.read_bytes()).hexdigest()
            == self.before_source,
            "Proxy changed while preparing isolation",
        )
        self.route_attempted = True
        self.reload(self.replacement)
        auth = self.root / "client-auth"
        auth.mkdir(mode=0o700)
        self.env["GOTOSOCIAL_FIXTURE_DIR"] = str(auth)
        self.env["GOTOSOCIAL_PASSWORD_FILE"] = str(self.root / "password.private")
        self.receipt.update(
            started=True,
            new_peer_id=self.peer_id,
            new_volume=volume,
            proxy_config_before_sha256=fingerprint(self.original),
            proxy_config_temporary_sha256=fingerprint(self.replacement),
            source_sha256=self.before_source,
            standing_before=self.standing_before,
            strict_tls_skip_verify=False,
        )
        self.save()

    def close(self):
        if self.route_attempted:
            active = self.active()
            require(
                active in (self.original, self.replacement),
                "Proxy changed outside isolated run; refuse overwrite",
            )
            require(
                hashlib.sha256(self.source.read_bytes()).hexdigest()
                == self.before_source,
                "Proxy source changed outside isolated run",
            )
            self.reload(self.original)
            self.receipt["proxy_original_restored"] = True
        if self.peer_id:
            info = self.inspect(self.peer_id)
            require(
                info["Config"]["Labels"].get("plamenu.validation.invocation")
                == self.nonce,
                "Owned peer labels changed; refuse stop",
            )
            self.command(["docker", "stop", "--time", "20", self.peer_id])
            require(
                not self.inspect(self.peer_id)["State"]["Running"],
                "New peer did not stop",
            )
            self.receipt["new_peer_stopped"] = True
        if self.standing_before is not None:
            require(
                self.snapshot(self.standing) == self.standing_before,
                "Standing peer lifecycle changed",
            )
        if self.proxy_before is not None:
            require(
                self.snapshot(self.proxy) == self.proxy_before,
                "Local proxy restarted unexpectedly",
            )
        self.receipt["standing_peer_lifecycle_unchanged"] = True
        self.receipt["proxy_lifecycle_unchanged"] = True
        self.save()
