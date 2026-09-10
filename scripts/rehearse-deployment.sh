#!/usr/bin/env bash
# Production-shaped native containers with disposable database/media state.
# A separate peer and TLS gateway survive destruction of the source stack.
set -euo pipefail
umask 077

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
MODE=${1:-restore}
case "$MODE" in
  install|restore) ;;
  *) echo "usage: $0 [install|restore]" >&2; exit 2 ;;
esac
for tool in docker git sha256sum diff; do
  command -v "$tool" >/dev/null 2>&1 || { echo "$tool is required" >&2; exit 1; }
done
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

WORK=$(mktemp -d)
SOURCE_PROJECT="plamenu-drill-${PPID}-${BASHPID}"
RESTORE_PROJECT="${SOURCE_PROJECT}-restore"
PEER_PROJECT="${SOURCE_PROJECT}-peer"
GATEWAY_PROJECT="${SOURCE_PROJECT}-gateway"
NETWORK="${SOURCE_PROJECT}-federation"
CLIENT="${SOURCE_PROJECT}-client"
CLIENT_VOLUME="${SOURCE_PROJECT}-client"
ACTIVE_PROJECT=$SOURCE_PROJECT
COMPOSE_FILE="$WORK/compose.yml"
GATEWAY_FILE="$WORK/gateway.yml"
REVISION=$(git -C "$ROOT" rev-parse HEAD)
case "$(uname -m)" in
  x86_64) EXPECTED_ARCH=amd64 ;;
  aarch64|arm64) EXPECTED_ARCH=arm64 ;;
  *) echo "unsupported native architecture" >&2; exit 1 ;;
esac
EXPECTED_ARCH=${PLAMENU_DRILL_ARCH:-$EXPECTED_ARCH}
case "$EXPECTED_ARCH" in amd64|arm64) ;; *) echo "unsupported image architecture" >&2; exit 1 ;; esac
IMAGE=${PLAMENU_DRILL_IMAGE:-"plamenu-deployment-drill:${REVISION}-${EXPECTED_ARCH}"}
PYTHON=python:3.13-alpine@sha256:7415fbc3c9e4979cc717d92377ab2bc7b2b4a2af1ac03cc52b5f3f88efedaf3a
ALPINE=alpine:3.22@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce

compose() {
  local endpoint=source
  [[ "$ACTIVE_PROJECT" == "$PEER_PROJECT" ]] && endpoint=peer
  DRILL_ENDPOINT=$endpoint docker_run compose --project-name "$ACTIVE_PROJECT" --file "$COMPOSE_FILE" "$@"
}
gateway() { docker_run compose --project-name "$GATEWAY_PROJECT" --file "$GATEWAY_FILE" "$@"; }
peer() { ACTIVE_PROJECT=$PEER_PROJECT compose "$@"; }
cleanup() {
  local result=$? project
  trap - EXIT
  if [[ $result -ne 0 ]]; then
    echo "FAIL deployment rehearsal; collecting disposable service logs" >&2
    compose logs --no-color --tail=80 >&2 || true
    peer logs --no-color --tail=80 >&2 || true
    gateway logs --no-color --tail=40 >&2 || true
  fi
  for project in "$SOURCE_PROJECT" "$RESTORE_PROJECT" "$PEER_PROJECT"; do
    ACTIVE_PROJECT=$project compose down --volumes --remove-orphans >/dev/null 2>&1 || true
  done
  gateway down --volumes --remove-orphans >/dev/null 2>&1 || true
  docker_run rm -f "$CLIENT" >/dev/null 2>&1 || true
  docker_run volume rm "$CLIENT_VOLUME" "${SOURCE_PROJECT}-source-config" "${SOURCE_PROJECT}-peer-config" >/dev/null 2>&1 || true
  docker_run network rm "$NETWORK" >/dev/null 2>&1 || true
  rm -rf -- "$WORK"
  exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for endpoint in source peer; do
  domain=restore-drill.test
  [[ "$endpoint" == peer ]] && domain=peer.restore-drill.test
  cat >"$WORK/$endpoint.toml" <<EOF
domain = "$domain"
bind = "0.0.0.0:8420"
database_url = "postgres://plamenu:deployment-drill@db/plamenu"
media_dir = "/var/lib/plamenu/media"
authorized_fetch = true
# Local fixture only: the dedicated internal network has no public egress.
allow_private_fetch = true
encryption_secret = "deployment-drill-$endpoint-envelope-root-not-for-production"
encryption_secret_version = 1
EOF
  # Disposable credentials; container UID 10001 must read the configuration.
  chmod 644 "$WORK/$endpoint.toml"
done
cat >"$WORK/Caddyfile" <<'EOF'
https://restore-drill.test {
    tls internal
    reverse_proxy source:8420
}
https://peer.restore-drill.test {
    tls internal
    reverse_proxy peer:8420
}
EOF
cat >"$GATEWAY_FILE" <<EOF
services:
  caddy:
    image: caddy:2.10-alpine@sha256:4c6e91c6ed0e2fa03efd5b44747b625fec79bc9cd06ac5235a779726618e530d
    command: ["caddy", "run", "--config", "/drill/Caddyfile", "--adapter", "caddyfile"]
    volumes:
      - client:/drill:ro
      - caddy-data:/data
      - caddy-config:/config
    networks:
      default:
      federation:
        aliases: [restore-drill.test, peer.restore-drill.test]
networks:
  default:
  federation:
    external: true
    name: $NETWORK
volumes:
  client:
    external: true
    name: $CLIENT_VOLUME
  caddy-data:
  caddy-config:
EOF
cat >"$COMPOSE_FILE" <<EOF
services:
  db:
    image: postgres:18-alpine@sha256:9a8afca54e7861fd90fab5fdf4c42477a6b1cb7d293595148e674e0a3181de15
    environment:
      POSTGRES_DB: plamenu
      POSTGRES_USER: plamenu
      POSTGRES_PASSWORD: deployment-drill
    volumes: ["postgres:/var/lib/postgresql"]
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U plamenu -d plamenu"]
      interval: 2s
      timeout: 3s
      retries: 30
  plamenu:
    image: $IMAGE
    platform: linux/$EXPECTED_ARCH
    init: true
    depends_on:
      db:
        condition: service_healthy
    environment:
      SSL_CERT_FILE: /etc/plamenu/drill-ca.crt
    volumes:
      - config:/etc/plamenu:ro
      - media:/var/lib/plamenu/media
    read_only: true
    tmpfs: ["/tmp:size=1g,mode=1777"]
    security_opt: ["no-new-privileges:true"]
    cap_drop: [ALL]
    networks:
      default:
      federation:
        aliases: ["\${DRILL_ENDPOINT}"]
networks:
  default:
    internal: true
  federation:
    external: true
    name: $NETWORK
volumes:
  config:
    external: true
    name: ${SOURCE_PROJECT}-\${DRILL_ENDPOINT}-config
  postgres:
  media:
EOF

if [[ ${PLAMENU_DRILL_SKIP_BUILD:-false} != true ]]; then
  echo ">> Building native $EXPECTED_ARCH image $IMAGE"
  docker_run build --tag "$IMAGE" \
    --build-arg PLAMENU_BUILD_CHANNEL=development --build-arg PLAMENU_BUILD_NUMBER=0 \
    --build-arg "PLAMENU_GIT_SHA=$REVISION" "$ROOT"
elif ! docker_run image inspect "$IMAGE" >/dev/null 2>&1; then
  docker_run pull "$IMAGE"
fi
ACTUAL_ARCH=$(docker_run image inspect "$IMAGE" --format '{{.Architecture}}')
[[ "$ACTUAL_ARCH" == "$EXPECTED_ARCH" ]] || { echo "image is not $EXPECTED_ARCH" >&2; exit 1; }
echo ">> Rehearsal source=$REVISION"
docker_run image inspect "$IMAGE" --format 'image={{.Id}} architecture={{.Architecture}} image_source={{index .Config.Labels "org.opencontainers.image.revision"}}'
# Pull before starting isolated networks; the application containers have no public egress.
docker_run pull "$ALPINE" >/dev/null
docker_run pull "$PYTHON" >/dev/null
docker_run volume create "$CLIENT_VOLUME" >/dev/null
for endpoint in source peer; do
  docker_run volume create "${SOURCE_PROJECT}-${endpoint}-config" >/dev/null
  docker_run run --rm -i --network none -v "${SOURCE_PROJECT}-${endpoint}-config:/config" \
    "$ALPINE" sh -c 'cat > /config/plamenu.toml; chmod 644 /config/plamenu.toml' <"$WORK/$endpoint.toml"
done
gateway pull
compose pull db

docker_run network create --internal "$NETWORK" >/dev/null
docker_run run --detach --name "$CLIENT" --network "$NETWORK" \
  --memory=256m --cpus=0.5 -v "$CLIENT_VOLUME:/drill" "$PYTHON" sleep infinity >/dev/null
docker_run cp "$WORK/Caddyfile" "$CLIENT:/drill/Caddyfile"
docker_run cp "$ROOT/scripts/rehearse-deployment-api.py" "$CLIENT:/drill/api.py"
gateway up --detach --wait
for _ in $(seq 1 60); do
  if gateway exec -T caddy cat /data/caddy/pki/authorities/local/root.crt >"$WORK/ca.crt" 2>/dev/null; then break; fi
  sleep 1
done
test -s "$WORK/ca.crt"
chmod 644 "$WORK/ca.crt"
docker_run cp "$WORK/ca.crt" "$CLIENT:/drill/ca.crt"
for endpoint in source peer; do
  docker_run run --rm -i --network none -v "${SOURCE_PROJECT}-${endpoint}-config:/config" \
    "$ALPINE" sh -c 'cat > /config/drill-ca.crt; chmod 644 /config/drill-ca.crt' <"$WORK/ca.crt"
done
wait_for_https() {
  local domain=$1
  for _ in $(seq 1 60); do
    if docker_run exec "$CLIENT" python3 -c '
import ssl, sys, urllib.request
with urllib.request.urlopen("https://" + sys.argv[1] + "/ready", context=ssl.create_default_context(cafile="/drill/ca.crt"), timeout=3) as response:
    assert response.status == 200
' "$domain" >/dev/null 2>&1; then return; fi
    sleep 1
  done
  echo "timed out waiting for verified HTTPS at $domain" >&2
  return 1
}
api_check() {
  docker_run exec "$CLIENT" python3 /drill/api.py "$1" \
    --connect-host restore-drill.test --port 443 --ca /drill/ca.crt --state /drill/client-state.json
}
media_volume() {
  docker_run volume ls --quiet --filter "label=com.docker.compose.project=$ACTIVE_PROJECT" \
    --filter 'label=com.docker.compose.volume=media'
}
sql() { compose exec -T db psql -X -v ON_ERROR_STOP=1 -U plamenu -d plamenu -Atc "$1"; }
counts() {
  sql "SELECT 'accounts', count(*) FROM accounts UNION ALL
       SELECT 'statuses', count(*) FROM statuses UNION ALL
       SELECT 'media_attachments', count(*) FROM media_attachments UNION ALL
       SELECT 'delivery_jobs', count(*) FROM delivery_jobs ORDER BY 1"
}
wait_for_empty_queue() {
  for _ in $(seq 1 120); do
    [[ $(sql 'SELECT count(*) FROM delivery_jobs') == 0 ]] && return
    sleep 1
  done
  echo "delivery queue did not drain" >&2
  return 1
}
report_resources() {
  local -a containers
  printf 'database_bytes=%s\n' "$(sql "SELECT pg_database_size('plamenu')")"
  mapfile -t containers < <(compose ps --quiet)
  docker_run stats --no-stream --format 'container={{.Name}} cpu={{.CPUPerc}} memory={{.MemUsage}}' "${containers[@]}"
}

echo ">> Starting clean source and independent peer"
compose up --detach --wait
peer up --detach --wait
wait_for_https restore-drill.test
wait_for_https peer.restore-drill.test
for project in "$SOURCE_PROJECT" "$PEER_PROJECT"; do
  (ACTIVE_PROJECT=$project; compose exec -T plamenu plamenu --config /etc/plamenu/plamenu.toml \
    account add drill --email "drill@$([[ "$project" == "$PEER_PROJECT" ]] && echo peer.restore-drill.test || echo restore-drill.test)" \
    --password deployment-drill-password)
done
api_check prepare
compose exec -T plamenu plamenu --config /etc/plamenu/plamenu.toml federation keys audit
wait_for_empty_queue
report_resources
echo "PASS clean-install architecture=$EXPECTED_ARCH (verified local-CA HTTPS)"
[[ "$MODE" == install ]] && exit 0

echo ">> Leaving a real delivery pending, then taking a stopped-write backup"
peer stop plamenu
api_check queue
compose stop plamenu >/dev/null
[[ $(sql 'SELECT count(*) FROM delivery_jobs') -gt 0 ]] || { echo 'no pending delivery to restore' >&2; exit 1; }
counts >"$WORK/counts-before"
compose exec -T db pg_dump -Fc -U plamenu plamenu >"$WORK/plamenu.dump"
docker_run run --rm --network none -v "$(media_volume):/data:ro" \
  "$ALPINE" tar -C /data -czf - . >"$WORK/media.tgz"
test -s "$WORK/plamenu.dump"
test -s "$WORK/media.tgz"
sha256sum "$WORK/plamenu.dump" "$WORK/media.tgz"

echo ">> Destroying source database and media volumes; restoring into a new project"
SOURCE_MEDIA=$(media_volume)
SOURCE_DB=$(docker_run volume ls --quiet --filter "label=com.docker.compose.project=$SOURCE_PROJECT" \
  --filter 'label=com.docker.compose.volume=postgres')
compose down --volumes --remove-orphans >/dev/null
for volume in "$SOURCE_MEDIA" "$SOURCE_DB"; do
  if docker_run volume inspect "$volume" >/dev/null 2>&1; then echo "source volume survived: $volume" >&2; exit 1; fi
done
ACTIVE_PROJECT=$RESTORE_PROJECT
compose up --detach --wait db
compose exec -T db pg_restore --exit-on-error -U plamenu -d plamenu <"$WORK/plamenu.dump"
counts >"$WORK/counts-after"
diff -u "$WORK/counts-before" "$WORK/counts-after"
cat "$WORK/counts-after"
compose create plamenu >/dev/null
docker_run run --rm -i --network none -v "$(media_volume):/data" \
  "$ALPINE" tar -C /data -xzf - <"$WORK/media.tgz"
# The original peer retains its database, keys, follows and cached source actor.
peer up --detach --wait plamenu
compose up --detach --wait plamenu
wait_for_https restore-drill.test
api_check restore
compose exec -T plamenu plamenu --config /etc/plamenu/plamenu.toml federation keys audit
wait_for_empty_queue
report_resources
echo "PASS backup-restore architecture=$EXPECTED_ARCH; restored queue drained to the surviving peer"
