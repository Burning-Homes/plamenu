#!/bin/ash
# shellcheck shell=dash
# Release entrypoint: wait for the DB, migrate, then boot Akkoma.
set -e

echo "-- Waiting for database..."
while ! pg_isready -U "${DB_USER:-pleroma}" \
    -d "postgres://${DB_HOST:-db}:5432/${DB_NAME:-pleroma}" -t 1; do
    sleep 1s
done

echo "-- Running migrations..."
/opt/akkoma/bin/pleroma_ctl migrate

echo "-- Starting!"
exec /opt/akkoma/bin/pleroma start
