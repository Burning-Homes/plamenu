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
measure python3 -m unittest discover -s scripts/tests
measure python3 -m pytest bench/test_bench_budgets.py -q
measure cargo deny --locked fetch all
measure cargo deny --locked check --deny warnings
measure cargo nextest run --locked --workspace --test-threads 2 --no-fail-fast --status-level fail --final-status-level fail
measure cargo check --locked --workspace --all-targets
measure ./scripts/check-docs.sh
# SQLx checks need the real database even though ordinary compilation is offline.
measure cargo sqlx migrate run --source crates/db/migrations
measure env SQLX_OFFLINE=false cargo sqlx prepare --workspace --check -- --all-targets
./scripts/check-public-tree.sh
printf 'PASS manual verification revision=%s architecture=%s\n' "$(git rev-parse HEAD)" "$(uname -m)"
