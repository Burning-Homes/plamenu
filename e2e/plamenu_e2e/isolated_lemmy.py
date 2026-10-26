"""Fresh Lemmy identity and volumes behind a guarded, temporary local route."""

import copy
import hashlib
import json
import re
import secrets
import subprocess
from pathlib import Path
from urllib.parse import urlsplit

from .isolated_gotosocial import require


def isolated_route(configuration, backend, frontend):
    replacement = copy.deepcopy(configuration)
    routes = replacement["apps"]["http"]["servers"]["srv0"]["routes"]
    matches = [r for r in routes if r.get("match") == [{"host": ["lemmy.local"]}]]
    require(len(matches) == 1, "Expected one exact Lemmy proxy route")
    substitutions = {"lemmy-web:8536": backend, "lemmy-ui-web:1234": frontend}
    seen = []

    def visit(value):
        if isinstance(value, dict):
            if value.get("handler") == "reverse_proxy":
                upstreams = value.get("upstreams")
                require(
                    isinstance(upstreams, list)
                    and len(upstreams) == 1
                    and upstreams[0].get("dial") in substitutions,
                    "Unexpected Lemmy upstream; refuse replacement",
                )
                dial = upstreams[0]["dial"]
                seen.append(dial)
                upstreams[0]["dial"] = substitutions[dial]
            for child in value.values():
                visit(child)
        elif isinstance(value, list):
            for child in value:
                visit(child)

    visit(matches[0])
    require(
        sorted(seen) == ["lemmy-ui-web:1234"] + ["lemmy-web:8536"] * 3,
        "Unexpected Lemmy route count",
    )
    return replacement


class IsolatedLemmy:
    def __init__(self, state, environment, output, *, peer_root, proxy):
        self.root = Path(state) / "lemmy-owned"
        self.root.mkdir(mode=0o700)
        self.fleet = Path(peer_root).resolve()
        self.env, self.output, self.proxy = environment, output, proxy
        self.nonce = secrets.token_hex(12)
        self.project = "plamenu-lemmy-" + self.nonce
        self.original = self.replacement = None
        self.route_attempted = False

    def command(self, args, *, capture=False, input=None):
        result = subprocess.run(
            args,
            env=self.env,
            text=True,
            input=input,
            stdout=subprocess.PIPE if capture else self.output,
            stderr=self.output,
            check=False,
        )
        require(
            result.returncode == 0, "Owned Lemmy command failed; inspect fixture log"
        )
        return result.stdout.strip() if capture else None

    def compose(self, *args, capture=False):
        return self.command(
            [
                "docker",
                "compose",
                "--project-directory",
                str(self.root),
                "--project-name",
                self.project,
                *args,
            ],
            capture=capture,
        )

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
        require(self.active() == value, "Proxy reload differs")

    def save(self, name, value):
        (self.root / name).write_text(json.dumps(value, indent=2) + "\n")
        (self.root / name).chmod(0o600)

    def start(self):
        proxy = self.inspect(self.proxy)
        self.proxy_id = proxy["Id"]
        sources = [
            m["Source"]
            for m in proxy["Mounts"]
            if m["Destination"] == "/etc/caddy/Caddyfile" and m["Type"] == "bind"
        ]
        require(len(sources) == 1, "Ambiguous proxy source")
        self.source = Path(sources[0])
        self.source_hash = hashlib.sha256(self.source.read_bytes()).hexdigest()
        self.original = self.active()
        self.save("proxy-before.private.json", self.original)
        config = json.loads(
            self.command(
                [
                    "docker",
                    "compose",
                    "--project-directory",
                    str(self.fleet / "lemmy-test"),
                    "-f",
                    str(self.fleet / "lemmy-test/compose.dev.yml"),
                    "config",
                    "--format",
                    "json",
                ],
                capture=True,
            )
        )
        config["name"] = self.project
        # Compose config expands default network names before this project is
        # renamed. Leaving that name intact would share database aliases with
        # a standing peer even though the volumes themselves are new.
        config["networks"]["lmy"] = {"name": self.project + "_private"}
        labels = {"plamenu.validation.invocation": self.nonce}
        config["volumes"] = {
            name: {"name": self.project + "_" + name, "labels": labels}
            for name in ("pgdata", "pictrs")
        }
        for volume in config["volumes"].values():
            require(
                subprocess.run(
                    ["docker", "volume", "inspect", volume["name"]],
                    check=False,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                ).returncode
                != 0,
                "Owned volume already exists",
            )
        for service in config["services"].values():
            service["restart"] = "no"
            service["labels"] = labels
            service.pop("build", None)
            service.pop("ports", None)
            service.pop("container_name", None)
            if "shared" in service["networks"]:
                # No standing/shared service alias can resolve to this fixture.
                service["networks"]["shared"] = {}
        self.save("docker-compose.yml", config)
        self.compose("up", "-d", "--wait", "--no-build", "--pull", "never")
        # Only the new owned database is configured. No queue/history reset.
        self.compose(
            "exec",
            "-T",
            "db",
            "psql",
            "-U",
            "lemmy",
            "-d",
            "lemmy",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            "UPDATE local_site SET federation_enabled=true,federation_signed_fetch=true,"
            "community_creation_admin_only=false,private_instance=false,registration_mode='Open',"
            "require_email_verification=false; UPDATE local_site_rate_limit SET "
            "message=9999,post=9999,register=9999,image=9999,comment=9999,search=9999;",
        )
        self.compose("restart", "lemmy")
        self.compose("up", "-d", "--wait", "--no-build", "--pull", "never")
        addresses = {}
        for service, port in [("lemmy", 8536), ("lemmy-ui", 1234)]:
            info = self.inspect(self.compose("ps", "-q", service, capture=True))
            require(
                info["Config"]["Labels"].get("plamenu.validation.invocation")
                == self.nonce,
                "Owned peer identity differs",
            )
            require(
                self.project + "_private" in info["NetworkSettings"]["Networks"],
                "Owned peer private network differs",
            )
            addresses[service] = (
                info["NetworkSettings"]["Networks"]["mastodon-test_internal"][
                    "IPAddress"
                ]
                + f":{port}"
            )
        self.replacement = isolated_route(
            self.original, addresses["lemmy"], addresses["lemmy-ui"]
        )
        require(self.active() == self.original, "Proxy changed during setup")
        self.route_attempted = True
        self.reload(self.replacement)
        self.env["LEMMY_FIXTURE_DIR"] = str(self.root)
        self.save(
            "ownership.json",
            {
                "project": self.project,
                "nonce": self.nonce,
                "fresh_volumes": config["volumes"],
                "full_pass": False,
            },
        )

    def prepare_federation(self, actor_uri):
        """Initialize the real remote-instance worker before test activities exist.

        Lemmy starts a new queue at the latest activity ID, intentionally skipping
        history. Resolving a real actor and waiting for that checkpoint avoids
        dropping the first test's Follow/Accept during worker initialization.
        """
        from .lemmy import FRANK_NICK, FRANK_PASS, LemmyApi
        from .steps import wait_for

        domain = urlsplit(actor_uri).hostname
        require(
            domain and re.fullmatch(r"[a-zA-Z0-9.-]+", domain), "Invalid actor host"
        )
        client = LemmyApi("https://lemmy.local")
        client.http.verify = str(self.fleet / "mastodon-test/ca-bundle.crt")
        token = client.login(FRANK_NICK, FRANK_PASS)
        client.http.headers["Authorization"] = f"Bearer {token}"
        require(
            "person" in client.resolve(actor_uri), "Bootstrap actor was not resolved"
        )
        # Restart only this new peer, before any case runs, to avoid the upstream
        # 60-second instance-discovery interval. No queue timestamps are edited.
        self.compose("restart", "lemmy")
        self.compose("up", "-d", "--wait", "--no-build", "--pull", "never")
        statement = (
            "SELECT EXISTS(SELECT 1 FROM federation_queue_state q JOIN instance i "
            "ON i.id=q.instance_id WHERE i.domain='" + domain + "' "
            "AND q.last_successful_id IS NOT NULL AND q.fail_count=0)"
        )
        wait_for(
            lambda: (
                self.compose(
                    "exec",
                    "-T",
                    "db",
                    "psql",
                    "-U",
                    "lemmy",
                    "-d",
                    "lemmy",
                    "-tA",
                    "-v",
                    "ON_ERROR_STOP=1",
                    "-c",
                    statement,
                    capture=True,
                )
                == "t"
            ),
            desc="owned Lemmy federation worker checkpoint",
        )
        self.save(
            "federation-ready.json",
            {
                "remote_domain": domain,
                "worker_checkpoint_ready": True,
                "full_pass": False,
            },
        )

    def close(self):
        if self.route_attempted:
            require(
                self.inspect(self.proxy)["Id"] == self.proxy_id,
                "Proxy replaced externally",
            )
            require(
                hashlib.sha256(self.source.read_bytes()).hexdigest()
                == self.source_hash,
                "Proxy source changed externally",
            )
            require(
                self.active() in (self.original, self.replacement),
                "Proxy changed externally",
            )
            self.reload(self.original)
        ids = self.command(
            [
                "docker",
                "ps",
                "-aq",
                "--filter",
                "label=plamenu.validation.invocation=" + self.nonce,
            ],
            capture=True,
        ).split()
        for identifier in ids:
            info = self.inspect(identifier)
            require(
                info["Config"]["Labels"].get("plamenu.validation.invocation")
                == self.nonce
                and info["Config"]["Labels"].get("com.docker.compose.project")
                == self.project,
                "Cleanup ownership differs",
            )
            if info["State"]["Running"]:
                self.command(["docker", "stop", "--time", "20", identifier])
            self.save(identifier + "-inspect.private.json", self.inspect(identifier))
            with (self.root / (identifier + "-logs.private.txt")).open("w") as log:
                subprocess.run(
                    ["docker", "logs", identifier],
                    stdout=log,
                    stderr=subprocess.STDOUT,
                    check=True,
                )
            # Keep named data volumes and exported diagnostics, but release
            # stopped endpoints so repeated runs cannot exhaust Docker's pool.
            self.command(["docker", "rm", identifier])
        networks = self.command(
            [
                "docker",
                "network",
                "ls",
                "-q",
                "--filter",
                "label=com.docker.compose.project=" + self.project,
            ],
            capture=True,
        ).split()
        for identifier in networks:
            info = self.inspect(identifier)
            require(
                info["Name"] == self.project + "_private"
                and info["Labels"].get("com.docker.compose.project") == self.project
                and not info["Containers"],
                "Cleanup network ownership or endpoints differ",
            )
            self.command(["docker", "network", "rm", identifier])
        self.save(
            "cleanup.json",
            {
                "owned_containers_stopped": True,
                "owned_containers_removed": True,
                "private_network_removed": True,
                "proxy_restored": self.original is not None
                and self.active() == self.original,
                "volumes_retained": True,
                "full_pass": False,
            },
        )
