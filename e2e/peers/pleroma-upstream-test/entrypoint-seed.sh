#!/bin/ash
# shellcheck shell=dash
# Upstream-Pleroma interop peer entrypoint. Seeds a test-only user config
# (trust the local Caddy CA on outbound TLS) into the data volume, then runs
# the normal migrate + start the upstream image would.
set -e

# docker.exs imports /var/lib/pleroma/config.exs last, so this overrides the
# baked defaults. verify_none: the interop peers terminate HTTPS with a private
# Caddy CA, so Pleroma must skip CA verification to fetch https://plamenu.local
# actors (signature check) and deliver its Accept back over HTTPS. Without this
# the Follow would be dropped for an unrelated (TLS) reason and mask the answer.
cat > /var/lib/pleroma/config.exs <<'EOF'
import Config

config :pleroma, :http, adapter: [ssl_options: [verify: :verify_none]]
EOF

echo "-- Waiting for database..."
while ! pg_isready -U "${DB_USER:-pleroma}" \
    -d "postgres://${DB_HOST:-db}:${DB_PORT:-5432}/${DB_NAME:-pleroma}" -t 1; do
    sleep 1s
done

echo "-- Running migrations..."
/opt/pleroma/bin/pleroma_ctl migrate

echo "-- Starting!"
exec /opt/pleroma/bin/pleroma start
