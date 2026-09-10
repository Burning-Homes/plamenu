"""Plamenu-side helpers: the cargo CLI and client-API login."""

import re
from dataclasses import dataclass, field

from . import config, shell, unique
from .api import Api, ApiError

OOB = "urn:ietf:wg:oauth:2.0:oob"


@dataclass(frozen=True)
class User:
    """A throwaway Plamenu account with login credentials.

    `domain` is the instance the account lives on — the standing dev server
    unless a second instance (`ephemeral.Instance`) created it."""

    username: str = field(default_factory=lambda: unique("e2e"))
    password: str = "e2e-password"
    domain: str = config.PLAMENU_DOMAIN

    @property
    def email(self) -> str:
        return f"{self.username}@{self.domain}"

    @property
    def acct(self) -> str:
        return f"{self.username}@{self.domain}"


class Cli:
    """The plamenu CLI (`cargo run -- ...`): local accounts, posting, following."""

    def __init__(self, run=None):
        """`run(*args) -> str` invokes the CLI. Defaults to the dev instance's
        `cargo run`; a second instance (`ephemeral.Instance.commands`) passes
        its own runner so every subcommand below works against it too."""
        self._runner = run

    def _run(self, *args: str) -> str:
        if self._runner is not None:
            return self._runner(*args)
        return shell.run(
            ["cargo", "run", "-q", "--", "--config", config.PLAMENU_CONFIG, *args],
            cwd=config.PLAMENU_DIR,
        )

    def account_add(self, user: User, display_name: str = "E2E Test") -> None:
        self._run(
            "account",
            "add",
            user.username,
            "--display-name",
            display_name,
            "--email",
            user.email,
            "--password",
            user.password,
        )

    def set_role(self, username: str, role: str) -> None:
        """Grant a moderation role (e.g. `Owner`) to bootstrap an admin."""
        self._run("account", "set-role", username, role)

    def post(self, username: str, text: str) -> None:
        self._run("post", username, text)

    def rotate_account_key(
        self,
        username: str,
        algorithm: str,
        overlap_hours: int = 1,
        activation_delay_seconds: int = 5,
    ) -> str:
        """Rotate one federation signing key while retaining the old key for
        a bounded verification overlap."""
        return self._run(
            "federation",
            "keys",
            "rotate-account",
            username,
            "--algorithm",
            algorithm,
            "--overlap-hours",
            str(overlap_hours),
            "--activation-delay-seconds",
            str(activation_delay_seconds),
        )

    def follow(self, username: str, target_acct: str) -> None:
        self._run("follow", username, target_acct)

    def emoji_add(self, shortcode: str, file: str) -> None:
        self._run("emoji", "add", shortcode, file)

    def group_add(
        self,
        name: str,
        owner: str,
        approval: bool = False,
        display_name: str | None = None,
    ) -> None:
        """Create a local group owned by an existing local account."""
        args = ["group", "add", name, "--owner", owner]
        if display_name is not None:
            args += ["--display-name", display_name]
        if approval:
            args.append("--approval")
        self._run(*args)

    def group_rename(self, group: str, display_name: str) -> None:
        """Rename a group — federates the profile Update(Group)."""
        self._run("group", "rename", group, "--display-name", display_name)

    def group_transfer(self, group: str, to: str) -> None:
        """Transfer group ownership to a local member."""
        self._run("group", "transfer", group, "--to", to)

    def group_delete(self, group: str) -> None:
        """Delete a group — federates Delete(Group) + Delete(Actor)."""
        self._run("group", "delete", group)

    def group_lock(self, group: str, status_id: int, unlock: bool = False) -> None:
        """Lock (or unlock) a group thread — moderator action."""
        self._run("group", "unlock" if unlock else "lock", group, str(status_id))

    def group_remove(self, group: str, status_id: int) -> None:
        """Remove a post from a group (mod removal)."""
        self._run("group", "remove", group, str(status_id))

    def group_ban(self, group: str, target: str, unban: bool = False) -> None:
        """Ban (or unban) an account from a group."""
        self._run("group", "unban" if unban else "ban", group, target)

    def alias_add(self, username: str, alias: str) -> str:
        """Declare an alias on `username`; returns the resolved actor URI."""
        out = self._run("account", "alias", "add", username, alias)
        # "alias added to @user: https://host/ap/users/123"
        i = out.find("https://")
        return out[i:].strip() if i != -1 else out.strip()

    def migrate(self, username: str, target: str) -> str:
        """Migrate `username` to `target`; returns the CLI output."""
        return self._run("account", "migrate", username, target).strip()


def login(user: User, scopes: str = "read write", base_url: str | None = None) -> Api:
    """Client-API token via the full OAuth flow: apps -> authorize -> token.

    `scopes` is the OAuth scope string; pass admin scopes (e.g.
    `"read write admin:read admin:write"`) to reach the `/admin/*` surface.
    Plamenu's scope grammar is free-form, so any space-separated set works.
    `base_url` targets a second Plamenu instance; it defaults to the standing
    dev server."""
    base_url = base_url or config.PLAMENU_URL
    api = Api(base_url)
    app = api.post(
        "/api/v1/apps",
        client_name="plamenu-e2e",
        redirect_uris=OOB,
        scopes=scopes,
    )
    page = api.http.post(
        f"{api.base_url}/oauth/authorize",
        data={
            "client_id": app["client_id"],
            "redirect_uri": OOB,
            "scope": scopes,
            "email": user.email,
            "password": user.password,
        },
        timeout=30,
    )
    code = re.search(r"<pre[^>]*>(.+?)</pre>", page.text)
    if not (page.ok and code):
        raise ApiError(
            f"/oauth/authorize gave no code ({page.status_code}) for {user.email}"
        )
    token = api.post(
        "/oauth/token",
        grant_type="authorization_code",
        code=code.group(1),
        client_id=app["client_id"],
        client_secret=app["client_secret"],
        redirect_uri=OOB,
    )["access_token"]
    return Api(base_url, token=token)


def admin() -> tuple[User, Api]:
    """Create a fresh local account, promote it to `Owner`, and return it
    with an admin-scoped client-API session — enough to reach `/admin/*`."""
    cli = Cli()
    user = User()
    cli.account_add(user)
    cli.set_role(user.username, "Owner")
    return user, login(user, scopes="read write admin:read admin:write")
