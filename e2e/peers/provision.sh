#!/usr/bin/env bash
# Provision every source-backed live federation peer from pinned revisions,
# create the shared local TLS trust bundle, and optionally start the required
# release matrix. Generated state remains below this ignored workspace.
set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH='' cd -- "$ROOT/../.." && pwd)
LOCK="$ROOT/sources.lock"
START=true
CHECK=false

case "${1:-}" in
  '') ;;
  --no-start) START=false ;;
  --check) START=false; CHECK=true ;;
  *) echo "usage: $0 [--no-start|--check]" >&2; exit 2 ;;
esac

for tool in git docker curl; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "$tool is required to provision federation peers" >&2
    exit 1
  }
done

clone_at() {
  local destination=$1 repository=$2 commit=$3
  local path="$ROOT/$destination"
  if [[ ! -d "$path/.git" ]]; then
    [[ "$CHECK" == false ]] || {
      echo "missing pinned source: $destination" >&2
      return 1
    }
    echo ">> Cloning $destination at $commit"
    mkdir -p "$(dirname -- "$path")"
    git init --quiet "$path"
    git -C "$path" remote add origin "$repository"
    git -C "$path" fetch --quiet --depth 1 origin "$commit"
    git -C "$path" checkout --quiet --detach FETCH_HEAD
  fi
  test "$(git -C "$path" remote get-url origin)" = "$repository" || {
    echo "$destination has the wrong origin" >&2
    return 1
  }
  test "$(git -C "$path" rev-parse HEAD)" = "$commit" || {
    echo "$destination is not at pinned commit $commit" >&2
    return 1
  }
}

while IFS='|' read -r destination repository commit; do
  case "$destination" in ''|'#'*) continue ;; esac
  clone_at "$destination" "$repository" "$commit"
done <"$LOCK"

ensure_submodules() {
  local checkout=$1 status
  status=$(git -C "$checkout" submodule status --recursive)
  if grep -q '^[+-U]' <<<"$status"; then
    [[ "$CHECK" == false ]] || {
      echo "pinned submodules are not prepared below: $checkout" >&2
      printf '%s\n' "$status" >&2
      return 1
    }
    echo ">> Initializing pinned submodules below $(basename -- "$checkout")"
    git -C "$checkout" submodule update --init --recursive --depth 1
  fi
}

# Lemmy keeps its compile-time translation assets in a pinned submodule. A
# source checkout at the right commit is still unbuildable until that exact
# nested revision has been initialized.
ensure_submodules "$ROOT/lemmy-source"

apply_once() {
  local checkout=$1 patch=$2
  if git -C "$checkout" apply --reverse --check "$patch" >/dev/null 2>&1; then
    return 0
  fi
  [[ "$CHECK" == false ]] || {
    echo "required patch is not applied: $patch" >&2
    return 1
  }
  git -C "$checkout" apply --check "$patch"
  git -C "$checkout" apply "$patch"
}

apply_once "$ROOT/lemmy-source" "$ROOT/patches/lemmy-native-roots.patch"
apply_once "$ROOT/pleroma-source" "$ROOT/patches/pleroma-dockerfile.patch"

if [[ "$CHECK" == true ]]; then
  echo "PASS pinned federation peer sources and patches"
  exit 0
fi

if [[ "$START" == false ]]; then
  echo "PASS federation peer sources prepared root=$ROOT"
  exit 0
fi

HOSTS=(
  mastodon.local plamenu.local webxdc.plamenu.local pleroma.local plup.local
  sharkey.local gotosocial.local mitra.local lemmy.local peertube.local
  owncast.local mobilizon.local discourse.local hubzilla.local funkwhale.local
  doomed.local plamenu2.local webxdc.plamenu2.local
)
missing_hosts=()
for host in "${HOSTS[@]}"; do
  getent hosts "$host" >/dev/null 2>&1 || missing_hosts+=("$host")
done
if ((${#missing_hosts[@]})); then
  echo ">> Adding disposable peer names to /etc/hosts (sudo may prompt)"
  printf '127.0.0.1 %s\n' "${missing_hosts[*]}" | sudo tee -a /etc/hosts >/dev/null
fi

if docker info >/dev/null 2>&1; then
  docker_run() { docker "$@"; }
elif sg docker -c 'docker info' >/dev/null 2>&1; then
  docker_run() {
    local quoted
    printf -v quoted '%q ' docker "$@"
    sg docker -c "$quoted"
  }
else
  docker_run() { sudo docker "$@"; }
fi

CA_BUNDLE="$ROOT/mastodon-test/ca-bundle.crt"
docker_run run --rm \
  alpine:3.22@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce \
  cat /etc/ssl/certs/ca-certificates.crt >"$CA_BUNDLE"

if [[ ! -f "$ROOT/mastodon-test/.env" ]]; then
  echo ">> Initializing Mastodon and the shared TLS terminator"
  (cd "$ROOT/mastodon-test" && ./masto setup)
else
  (cd "$ROOT/mastodon-test" && ./masto up)
fi

for _ in $(seq 1 60); do
  if docker_run compose --project-directory "$ROOT/mastodon-test" exec -T caddy \
    test -s /data/caddy/pki/authorities/local/root.crt; then
    break
  fi
  sleep 1
done
docker_run compose --project-directory "$ROOT/mastodon-test" exec -T caddy \
  cat /data/caddy/pki/authorities/local/root.crt >>"$CA_BUNDLE"
docker_run compose --project-directory "$ROOT/mastodon-test" \
  restart web sidekiq streaming >/dev/null

LOCAL_ENV="$REPO/.env"
[[ -f "$LOCAL_ENV" ]] || cp "$REPO/.env.example" "$LOCAL_ENV"
for setting in \
  "PLAMENU_PEER_ROOT=$ROOT" \
  "PLAMENU_PEER_DEV=$ROOT/dev" \
  "SSL_CERT_FILE=$CA_BUNDLE" \
  "REQUESTS_CA_BUNDLE=$CA_BUNDLE"; do
  key=${setting%%=*}
  if grep -q "^${key}=" "$LOCAL_ENV"; then
    sed -i "s|^${key}=.*|${setting}|" "$LOCAL_ENV"
  else
    printf '%s\n' "$setting" >>"$LOCAL_ENV"
  fi
done

if [[ "$START" == true ]]; then
  echo ">> Starting and seeding the required federation matrix"
  for peer in discourse hubzilla funkwhale pleroma sharkey gotosocial mitra lemmy peertube owncast; do
    "$ROOT/dev" up "$peer"
  done
fi

echo "PASS federation peer provisioning root=$ROOT"
