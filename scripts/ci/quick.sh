#!/bin/sh
set -eu
cargo fmt --all -- --check
shellcheck dev scripts/*.sh scripts/ci/*.sh docs/examples/*.sh \
  e2e/peers/dev e2e/peers/provision.sh \
  e2e/peers/discourse-test/entrypoint.sh e2e/peers/hubzilla-test/entrypoint.sh \
  e2e/peers/pleroma-test/docker-entrypoint.sh e2e/peers/pleroma-upstream-test/entrypoint-seed.sh
python3 scripts/ci/check-static.py
python3 scripts/accessibility-evidence.py matrix
ruff check e2e
ruff format --check e2e
cargo deny --locked fetch all
cargo deny --locked check --deny warnings
python3 scripts/ci/check-contribution.py
gitleaks git --redact --no-banner .
./scripts/check-public-tree.sh
