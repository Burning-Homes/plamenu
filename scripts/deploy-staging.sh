#!/usr/bin/env bash
# Build a static release binary, atomically install it in an Incus staging
# container, restart its systemd service, and verify the public endpoint.
set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

ENV_FILE=${PLAMENU_STAGING_ENV:-"$ROOT/.staging.env"}
if [[ -f "$ENV_FILE" ]]; then
  set -a
  # shellcheck disable=SC1090 # The operator explicitly selects this local file.
  . "$ENV_FILE"
  set +a
fi

required=(
  PLAMENU_STAGING_SSH_HOST
  PLAMENU_STAGING_CONTAINER
  PLAMENU_STAGING_DOMAIN
)
for name in "${required[@]}"; do
  [[ -n ${!name:-} ]] || {
    echo "Missing $name; copy .staging.env.example to .staging.env" >&2
    exit 2
  }
done

SSH_HOST=$PLAMENU_STAGING_SSH_HOST
CONTAINER=$PLAMENU_STAGING_CONTAINER
DOMAIN=$PLAMENU_STAGING_DOMAIN
SERVICE=${PLAMENU_STAGING_SERVICE:-plamenu}
OWNER=${PLAMENU_STAGING_OWNER:-$SERVICE}
GROUP=${PLAMENU_STAGING_GROUP:-$SERVICE}
TARGET=${PLAMENU_STAGING_TARGET:-x86_64-unknown-linux-musl}
REMOTE_BIN=${PLAMENU_STAGING_REMOTE_BIN:-/opt/plamenu/plamenu}
HEALTH_PATH=${PLAMENU_STAGING_HEALTH_PATH:-/api/v1/instance}
HEALTH_ATTEMPTS=${PLAMENU_STAGING_HEALTH_ATTEMPTS:-30}
UNSTRIPPED_BIN="target/$TARGET/deploy/plamenu"
BIN="target/$TARGET/deploy/plamenu.stripped"
STRIP_TOOL=${PLAMENU_STAGING_STRIP:-llvm-strip}
UPLOAD="/tmp/${SERVICE}.deploy.$$"
BUILD_NUMBER=${PLAMENU_BUILD_NUMBER:-$(git rev-list --count HEAD)}
GIT_SHA=${PLAMENU_GIT_SHA:-$(git rev-parse HEAD)}
if [[ -n $(git status --porcelain --untracked-files=no) ]]; then
  GIT_DIRTY=true
else
  GIT_DIRTY=false
fi

say() { printf '\033[1;34m>>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m!!\033[0m %s\n' "$*" >&2; exit 1; }

[[ "$CONTAINER" =~ ^[A-Za-z0-9_.-]+$ ]] || die "unsafe container name: $CONTAINER"
if [[ ! "$SSH_HOST" =~ ^([A-Za-z0-9_.-]+@)?[A-Za-z0-9.-]+$ ]] \
  || [[ "$SSH_HOST" == -* ]] \
  || [[ "$SSH_HOST" == *..* ]]; then
  die "unsafe SSH host: $SSH_HOST"
fi
if [[ ! "$DOMAIN" =~ ^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$ ]] \
  || [[ "$DOMAIN" != *.* ]] \
  || [[ "$DOMAIN" == *..* ]]; then
  die "unsafe staging domain: $DOMAIN"
fi
[[ "$SERVICE" =~ ^[A-Za-z0-9_.@-]+$ ]] || die "unsafe service name: $SERVICE"
[[ "$OWNER" =~ ^[A-Za-z0-9_.-]+$ ]] || die "unsafe owner name: $OWNER"
[[ "$GROUP" =~ ^[A-Za-z0-9_.-]+$ ]] || die "unsafe group name: $GROUP"
[[ "$REMOTE_BIN" =~ ^/[A-Za-z0-9_./-]+$ ]] || die "unsafe binary path: $REMOTE_BIN"
[[ "$HEALTH_PATH" == /* ]] || die "health path must start with /"
[[ "$HEALTH_ATTEMPTS" =~ ^[0-9]+$ ]] || die "health attempts must be numeric"

command -v cargo >/dev/null || die "cargo is not installed"
command -v rustup >/dev/null || die "rustup is not installed"
command -v scp >/dev/null || die "scp is not installed"
command -v ssh >/dev/null || die "ssh is not installed"
command -v curl >/dev/null || die "curl is not installed"
command -v "$STRIP_TOOL" >/dev/null || die "binary strip tool is missing: $STRIP_TOOL"
rustup target list --installed 2>/dev/null | grep -qx "$TARGET" \
  || die "Rust target $TARGET is not installed: rustup target add $TARGET"

case "$TARGET" in
  x86_64-unknown-linux-musl) linker=${PLAMENU_STAGING_LINKER:-x86_64-linux-musl-gcc} ;;
  aarch64-unknown-linux-musl) linker=${PLAMENU_STAGING_LINKER:-aarch64-linux-musl-gcc} ;;
  *) linker=${PLAMENU_STAGING_LINKER:-} ;;
esac
[[ -z "$linker" ]] || command -v "$linker" >/dev/null \
  || die "musl linker is missing: $linker"

say "Building locked $TARGET staging binary (build $BUILD_NUMBER)"
if [[ -n ${PLAMENU_STAGING_RUSTFLAGS:-} ]]; then
  export RUSTFLAGS=$PLAMENU_STAGING_RUSTFLAGS
fi
PLAMENU_BUILD_CHANNEL=staging \
PLAMENU_BUILD_NUMBER=$BUILD_NUMBER \
PLAMENU_GIT_SHA=$GIT_SHA \
PLAMENU_GIT_DIRTY=$GIT_DIRTY \
SQLX_OFFLINE=true \
  cargo build --locked --profile deploy --target "$TARGET" -p plamenu
[[ -s "$UNSTRIPPED_BIN" ]] || die "build produced no binary at $UNSTRIPPED_BIN"
install -m 0755 "$UNSTRIPPED_BIN" "$BIN"
"$STRIP_TOOL" --strip-all "$BIN"
say "Built $(du -h "$BIN" | cut -f1) stripped static binary"
say "Diagnostic binary retained at $UNSTRIPPED_BIN"

say "Uploading to $SSH_HOST -> $CONTAINER:$REMOTE_BIN"
scp -q -- "$BIN" "$SSH_HOST:$UPLOAD"

remote_env=$(printf \
  'CONTAINER=%q SERVICE=%q OWNER=%q GROUP=%q REMOTE_BIN=%q UPLOAD=%q' \
  "$CONTAINER" "$SERVICE" "$OWNER" "$GROUP" "$REMOTE_BIN" "$UPLOAD")
ssh -- "$SSH_HOST" "$remote_env bash -s" <<'REMOTE'
set -euo pipefail

cleanup() { rm -f -- "$UPLOAD"; }
trap cleanup EXIT

incus file push "$UPLOAD" "${CONTAINER}${REMOTE_BIN}.new"
incus exec "$CONTAINER" -- bash -s -- \
  "$SERVICE" "$OWNER" "$GROUP" "$REMOTE_BIN" <<'CONTAINER_REMOTE'
set -euo pipefail
service=$1
owner=$2
group=$3
binary=$4
previous="${binary}.previous"
unit=$service
case "$unit" in
  *.service) ;;
  *) unit="${unit}.service" ;;
esac
drop_in="/etc/systemd/system/${unit}.d"

if [[ -f "$binary" ]]; then
  cp -p -- "$binary" "$previous"
fi
install -o "$owner" -g "$group" -m 0755 "${binary}.new" "$binary"
rm -f -- "${binary}.new"
install -d -m 0755 "$drop_in"
printf '[Service]\nEnvironment=MIMALLOC_ALLOW_THP=0\n' \
  >"$drop_in/10-plamenu-memory.conf"
systemctl daemon-reload
systemctl restart "$service"

state=unknown
for _ in $(seq 1 30); do
  state=$(systemctl is-active "$service" || true)
  [[ "$state" == active ]] && exit 0
  sleep 1
done

echo "service failed after deploy; recent log follows" >&2
journalctl -u "$service" -n 50 --no-pager >&2 || true
if [[ -f "$previous" ]]; then
  echo "rolling back to previous binary" >&2
  install -o "$owner" -g "$group" -m 0755 "$previous" "$binary"
  systemctl restart "$service"
fi
exit 1
CONTAINER_REMOTE
REMOTE

say "Verifying https://$DOMAIN$HEALTH_PATH"
code=000
for _ in $(seq 1 "$HEALTH_ATTEMPTS"); do
  code=$(curl -sS --max-time 25 -o /dev/null -w '%{http_code}' \
    "https://$DOMAIN$HEALTH_PATH" || true)
  [[ "$code" == 200 ]] && break
  sleep 1
done
[[ "$code" == 200 ]] \
  || die "health check failed (HTTP $code); inspect $SERVICE in $CONTAINER"
say "Staging is live at https://$DOMAIN (HTTP $code)"
