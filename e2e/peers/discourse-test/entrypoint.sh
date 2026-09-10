#!/usr/bin/env bash
set -euo pipefail

cd /src

# Named volumes are created as root. The source checkout and all generated
# development files should remain owned by the normal Discourse user.
mkdir -p /bundle /src/node_modules /src/public/uploads
chown discourse:discourse /bundle /src/node_modules /src/public/uploads

echo ">> Checking Ruby dependencies..."
runuser -u discourse -- bundle check || runuser -u discourse -- bundle install

echo ">> Checking JavaScript dependencies..."
# The root store can survive while the pinned source checkout is replaced.
# Workspace package links live in that checkout, so a matching store lockfile
# alone does not prove the install is usable.
if [[ ! -f node_modules/.pnpm/lock.yaml ]] \
  || ! cmp -s pnpm-lock.yaml node_modules/.pnpm/lock.yaml \
  || [[ ! -e frontend/asset-processor/node_modules/rolldown ]]; then
  runuser -u discourse -- pnpm install
fi

echo ">> Preparing the development database..."
runuser -u discourse -- bin/rails db:create
runuser -u discourse -- bin/rails db:migrate

echo ">> Applying the disposable ActivityPub fixture..."
runuser -u discourse -- bin/rails runner /opt/discourse-test/seed.rb

# The watcher persists its PID in this generated file but does not remove it
# on shutdown. Container PID reuse can otherwise make a clean restart look as
# though a second watcher is already running.
rm -f /src/frontend/discourse/dist/manifest/build.json

echo ">> Starting Discourse web/frontend with its supervised Sidekiq worker..."
exec setpriv --reuid=discourse --regid=discourse --init-groups bin/dev
