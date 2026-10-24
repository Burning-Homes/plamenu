#!/bin/bash
set -euo pipefail
mkdir -p .ci/evidence
exec > >(tee .ci/evidence/verify.log) 2>&1
printf 'revision=%s\narchitecture=%s\n' "$(git rev-parse HEAD)" "$(uname -m)"
rustc --version
cargo nextest --version
ffmpeg -version | head -n 1
for _attempt in $(seq 1 30); do
  if pg_isready -h postgres -U plamenu -d plamenu_ci; then break; fi
  sleep 1
done
psql "$DATABASE_URL" -XAtc 'select version()'
measure() { /usr/bin/time -v "$@"; }
measure npm --prefix accessibility ci --ignore-scripts
measure python3 scripts/test-harness.py --profile portable
measure python3 scripts/update-gate-inventory.py --check-sources
measure python3 e2e/tools/federation_coverage.py
measure python3 scripts/accessibility-evidence.py matrix
measure cargo deny --locked fetch all
measure cargo deny --locked check --deny warnings
# This service database is disposable and exclusive to this verification job.
# Upgrade tests clear their own schemas; ordinary SQLx tests clone template1
# and validate the real migration ledger before running their assertions.
DATABASE_URL="${DATABASE_URL%/*}/template1" measure cargo sqlx migrate run --source crates/db/migrations
measure cargo nextest run --locked --workspace --test-threads 1 --no-fail-fast --status-level fail --final-status-level fail
measure ./scripts/check-docs.sh
# The fresh online SQLx pass also type-checks every workspace target, including
# benches. Nextest above already compiled ordinary code/tests offline; repeating
# cargo check here would type-check the same targets again before this pass.
measure cargo sqlx migrate run --source crates/db/migrations
measure env SQLX_OFFLINE=false cargo sqlx prepare --workspace --check -- --all-targets
./scripts/check-public-tree.sh
printf 'PASS manual verification revision=%s architecture=%s\n' "$(git rev-parse HEAD)" "$(uname -m)"
