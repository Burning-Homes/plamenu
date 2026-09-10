"""Mastodon-side helpers: API tokens, state probes and throwaway accounts.

Both go through `docker compose run web` in the mastodon-test stack, which
is slow (~10s a call) — mint tokens once per session (the `alice` fixture).
"""

from . import config, shell
from .api import Api


def _safe_identifier(value: str) -> str:
    """Keep values interpolated into test-only SQL deliberately boring."""
    if not value or not all(ch.isalnum() or ch in "_.-" for ch in value):
        raise ValueError(f"unsafe Mastodon test identifier: {value!r}")
    return value


def remote_follows_local(
    remote_username: str,
    remote_domain: str,
    local_username: str,
) -> bool:
    """Whether Mastodon's authoritative receiving state contains a Follow.

    The relationships API can briefly retain a negative relationship computed
    immediately after resolving a new remote account.  Federation acceptance
    tests care about the receiver's committed state, so probe the disposable
    Mastodon database directly instead of treating that API cache as truth.
    """
    remote_username = _safe_identifier(remote_username)
    remote_domain = _safe_identifier(remote_domain)
    local_username = _safe_identifier(local_username)
    query = (
        "SELECT EXISTS ("
        "SELECT 1 FROM follows f "
        "JOIN accounts remote ON remote.id = f.account_id "
        "JOIN accounts local ON local.id = f.target_account_id "
        f"WHERE remote.username = '{remote_username}' "
        f"AND remote.domain = '{remote_domain}' "
        f"AND local.username = '{local_username}' AND local.domain IS NULL"
        ")"
    )
    out = shell.masto_compose(
        "exec",
        "-T",
        "db",
        "psql",
        "-U",
        "mastodon",
        "-d",
        "mastodon_production",
        "-tA",
        "-c",
        query,
    )
    values = [line.strip() for line in out.splitlines() if line.strip()]
    if not values or values[-1] not in {"t", "f"}:
        raise RuntimeError(f"Mastodon follow probe returned no boolean: {out[-500:]}")
    return values[-1] == "t"


_TOKEN_RUNNER = """
user = User.find_by(email: '%s')
app = Doorkeeper::Application.find_or_create_by!(name: 'e2e') { |a| a.redirect_uri = 'urn:ietf:wg:oauth:2.0:oob'; a.scopes = 'read write follow' }
puts Doorkeeper::AccessToken.find_or_create_by!(application_id: app.id, resource_owner_id: user.id, scopes: 'read write follow').token
"""


def mint_token(email: str) -> str:
    """An API token for an existing Mastodon user, via a Doorkeeper rails-runner."""
    out = shell.masto_compose(
        "run", "--rm", "-T", "web", "bin/rails", "runner", _TOKEN_RUNNER % email
    )
    lines = [line for line in out.splitlines() if line.strip()]
    if not lines:
        raise RuntimeError(f"rails runner printed no token for {email}")
    return lines[-1].strip()


def api_as(email: str) -> Api:
    return Api(config.MASTODON_URL, token=mint_token(email))


_CLEAR_COLLECTIONS_RUNNER = r"""
require 'sidekiq/api'

account = Account.find_local('%s')
abort 'missing local account' unless account
actor_uri = ActivityPub::TagManager.instance.uri_for(account)

# The pinned Mastodon nightly currently passes the local collection owner's
# blank inbox URL to DeliveryWorker when DeleteCollectionService retracts a
# remote member.  It also lets an Add distribution race deletion of the
# collection row.  E2E housekeeping must not feed those known upstream bugs
# into Sidekiq's retry set, so remove only this fixture owner's old collection
# jobs and destroy the disposable rows without invoking the federation service.
sets = Sidekiq::Queue.all + [
  Sidekiq::RetrySet.new,
  Sidekiq::ScheduledSet.new,
  Sidekiq::DeadSet.new,
]
removed_jobs = 0
sets.each do |set|
  set.each do |job|
    workers = ['ActivityPub::CollectionRawDistributionWorker', 'ActivityPub::DeliveryWorker']
    next unless workers.include?(job.klass)
    payload = job.args.first.to_s
    next unless payload.include?(actor_uri) && payload.include?('/collections/')

    job.delete
    removed_jobs += 1
  end
end

removed_collections = account.collections.count
account.collections.find_each(&:destroy!)
puts "collections-cleared #{removed_collections} jobs-cleared #{removed_jobs}"
"""


def clear_collections(username: str) -> None:
    """Locally clear a fixture account's disposable Mastodon collections.

    This deliberately bypasses Mastodon's federation deletion service. The
    alpha account-collections implementation in our pinned nightly queues
    malformed cleanup deliveries for remote members; API cleanup would leave
    deterministic errors retrying throughout every subsequent E2E run.
    """
    username = _safe_identifier(username)
    out = shell.masto_compose(
        "run",
        "--rm",
        "-T",
        "web",
        "bin/rails",
        "runner",
        _CLEAR_COLLECTIONS_RUNNER % username,
    )
    if "collections-cleared" not in out:
        raise RuntimeError(f"collection cleanup printed no confirmation: {out[-500:]}")


_EMOJI_RUNNER = """
require 'base64'
File.binwrite('/tmp/e2e_emoji.png', Base64.decode64('%s'))
emoji = CustomEmoji.create!(shortcode: '%s', image: File.open('/tmp/e2e_emoji.png'))
puts "emoji-created #{emoji.id}"
"""


def create_emoji(shortcode: str, png_base64: str) -> None:
    """A local custom emoji on the test Mastodon, via a rails runner."""
    out = shell.masto_compose(
        "run",
        "--rm",
        "-T",
        "web",
        "bin/rails",
        "runner",
        _EMOJI_RUNNER % (png_base64, shortcode),
    )
    if "emoji-created" not in out:
        raise RuntimeError(f"emoji creation printed no confirmation: {out[-500:]}")


_REPORT_COUNT_RUNNER = """
target = Account.find_by(username: '%s', domain: nil)
count = target ? Report.where(target_account_id: target.id).where("comment LIKE ?", "%%%s%%").count : -1
puts "report-count #{count}"
"""


def report_count_about(username: str, marker: str) -> int:
    """How many Mastodon `reports` target the local account `username` and
    carry `marker` in their comment — what a `Flag` Plamenu forwarded to us
    must produce. Via a rails-runner (slow; poll sparingly)."""
    out = shell.masto_compose(
        "run",
        "--rm",
        "-T",
        "web",
        "bin/rails",
        "runner",
        _REPORT_COUNT_RUNNER % (username, marker),
    )
    for line in out.splitlines():
        line = line.strip()
        if line.startswith("report-count "):
            return int(line.removeprefix("report-count "))
    raise RuntimeError(f"report-count runner printed no count: {out[-500:]}")


def create_account(username: str) -> None:
    """A fresh confirmed Mastodon account (tootctl — rails Account.create!
    yields a 404 actor, do not switch back)."""
    shell.masto_compose(
        "run",
        "--rm",
        "-T",
        "web",
        "bin/tootctl",
        "accounts",
        "create",
        username,
        "--email",
        f"{username}@mastodon.local",
        "--confirmed",
        "--approve",
    )


_SUSPEND_RUNNER = """
account = Account.find_local('%s')
account.suspend!(origin: :local)
SuspendAccountService.new.call(account)
puts "suspended #{account.reload.suspended?}"
"""


def suspend_account(username: str) -> None:
    """Admin-suspends the local Mastodon account `username` (rails runner —
    the admin flow's `suspend!` + `SuspendAccountService`, which distributes
    the blanked `Update(Actor)` carrying `suspended: true` to followers)."""
    out = shell.masto_compose(
        "run",
        "--rm",
        "-T",
        "web",
        "bin/rails",
        "runner",
        _SUSPEND_RUNNER % username,
    )
    if "suspended true" not in out:
        raise RuntimeError(f"suspend runner failed: {out[-500:]}")


_UNSUSPEND_RUNNER = """
account = Account.find_local('%s')
account.unsuspend!
UnsuspendAccountService.new.call(account)
puts "unsuspended #{!account.reload.suspended?}"
"""


def unsuspend_account(username: str) -> None:
    """Lifts a local Mastodon suspension (rails runner — `unsuspend!` +
    `UnsuspendAccountService`, which re-distributes the restored actor)."""
    out = shell.masto_compose(
        "run",
        "--rm",
        "-T",
        "web",
        "bin/rails",
        "runner",
        _UNSUSPEND_RUNNER % username,
    )
    if "unsuspended true" not in out:
        raise RuntimeError(f"unsuspend runner failed: {out[-500:]}")


_ATTRIBUTION_RUNNER = """
account = Account.find_by(username: '%s', domain: '%s')
puts "attribution-domains #{account ? account.attribution_domains.join(',') : 'missing'}"
"""


def remote_attribution_domains(username: str, domain: str) -> str:
    """The attribution domains Mastodon has stored for a remote account, as a
    comma-joined string ('' when empty, 'missing' when the account is
    unknown). Via a rails runner (slow; poll sparingly)."""
    out = shell.masto_compose(
        "run",
        "--rm",
        "-T",
        "web",
        "bin/rails",
        "runner",
        _ATTRIBUTION_RUNNER % (username, domain),
    )
    for line in out.splitlines():
        line = line.strip()
        if line.startswith("attribution-domains"):
            return line.removeprefix("attribution-domains").strip()
    raise RuntimeError(f"attribution runner printed nothing: {out[-500:]}")


_ALIAS_RUNNER = """
account = Account.find_local('%s')
account_alias = AccountAlias.create!(account: account, acct: '%s')
puts "alias-created #{account_alias.uri}"
"""


_TAG_COLLECTION_RUNNER = """
status = Status.find(%s)
collection = Collection.find(%s)
status.tagged_objects.find_or_create_by!(object: collection) do |t|
  t.ap_type = 'FeaturedCollection'
  t.uri = ActivityPub::TagManager.instance.uri_for(collection)
end
# Drop the cached ActivityPub payload so it is regenerated with the new tag on
# the next fetch. StatusesController#show renders it via render_with_cache under
# `statuses/show:<status.cache_key>`; cache_versioning is on (the key omits
# updated_at), so `touch` alone would NOT bust it — an explicit delete is
# required, else a resolve racing the injection sees the stale, tag-less payload.
Rails.cache.delete(["statuses/show", status.cache_key].join(':'))
puts "tagged #{status.tagged_objects.count}"
"""


def tag_status_with_collection(status_id: str, collection_id: str) -> None:
    """Attach a FeaturedCollection tag to a Mastodon status via a rails runner.
    Mastodon never linkifies `.local` URLs, so its ProcessLinksService won't
    auto-tag a local collection; this injects the TaggedObject directly."""
    out = shell.masto_compose(
        "run",
        "--rm",
        "-T",
        "web",
        "bin/rails",
        "runner",
        _TAG_COLLECTION_RUNNER % (status_id, collection_id),
    )
    if "tagged" not in out:
        raise RuntimeError(
            f"tag_status_with_collection printed no confirmation: {out[-500:]}"
        )


_MOVE_RUNNER = """
account = Account.find_local('%s')
migration = account.migrations.create!(acct: '%s')
MoveService.new.call(migration)
puts "moved-to #{ActivityPub::TagManager.instance.uri_for(account.reload.moved_to_account)}"
"""


def trigger_move(mover_username: str, dest_acct: str) -> str:
    """Move a local Mastodon account to `dest_acct` (rails: AccountMigration +
    MoveService, which bypasses the password challenge but still validates that
    dest lists the mover in `alsoKnownAs`). Federates the `Move` to the mover's
    remote followers; returns the destination actor URI."""
    out = shell.masto_compose(
        "run",
        "--rm",
        "-T",
        "web",
        "bin/rails",
        "runner",
        _MOVE_RUNNER % (mover_username, dest_acct),
    )
    for line in out.splitlines():
        line = line.strip()
        if line.startswith("moved-to "):
            return line.removeprefix("moved-to ").strip()
    raise RuntimeError(f"trigger_move printed no destination uri: {out[-500:]}")


_DELETE_ACCOUNT_RUNNER = """
account = Account.find_local('%s')
DeleteAccountService.new.call(account)
puts "delete-called %s"
"""


def delete_account(username: str) -> None:
    """Delete a (confirmed) local Mastodon account (rails: DeleteAccountService,
    which federates a signed `Delete(Actor)` to followers + all known inboxes).
    An unconfirmed account would set skip_side_effects and suppress federation."""
    out = shell.masto_compose(
        "run",
        "--rm",
        "-T",
        "web",
        "bin/rails",
        "runner",
        _DELETE_ACCOUNT_RUNNER % (username, username),
    )
    if "delete-called" not in out:
        raise RuntimeError(f"delete_account printed no confirmation: {out[-500:]}")


_REMOVE_FOLLOWER_RUNNER = """
follower = Account.find_remote('%s', '%s')
target = Account.find_local('%s')
follow = follower && Follow.find_by(account: follower, target_account: target)
follow&.destroy
puts "removed #{follow ? 'yes' : 'no'}"
"""


def remove_follower(follower_acct: str, target_username: str) -> None:
    """Destroy the `Follow` row where `follower_acct` follows the local
    `target_username` (rails). `.destroy` (not delete) runs the after-commit
    hook that invalidates the cached followers hash, so the next
    Collection-Synchronization header carries a fresh (empty) digest —
    simulating a missed Undo(Follow) that leaves a ghost on the *other* side."""
    username, _, domain = follower_acct.partition("@")
    out = shell.masto_compose(
        "run",
        "--rm",
        "-T",
        "web",
        "bin/rails",
        "runner",
        _REMOVE_FOLLOWER_RUNNER % (username, domain, target_username),
    )
    if "removed yes" not in out:
        raise RuntimeError(
            f"remove_follower did not remove the follow row: {out[-500:]}"
        )


def add_alias(username: str, acct: str) -> str:
    """Declares `acct` as an alias of the local Mastodon account `username`
    (via a rails runner — Mastodon has no API for aliases). Resolves `acct`
    over live webfinger, so the aliased peer must be up. Returns the alias
    URI Mastodon stored (what its actor doc now lists in `alsoKnownAs`)."""
    out = shell.masto_compose(
        "run",
        "--rm",
        "-T",
        "web",
        "bin/rails",
        "runner",
        _ALIAS_RUNNER % (username, acct),
    )
    for line in out.splitlines():
        line = line.strip()
        if line.startswith("alias-created "):
            return line.removeprefix("alias-created ")
    raise RuntimeError(f"alias runner failed: {out[-500:]}")
