#!/bin/sh
# A change to the benchmark dataset must bump SEED_VERSION, or every machine
# that already has a `plamenu_bench` database keeps measuring the old shape.
#
# This has gone wrong once already: 80 `with_replies = false` follows were added
# to the dense persona without a bump, so the live database carried the 13 rows
# a migration had backfilled and the arm ran at ~14% of its intended volume for
# thirteen hours — while the bench reported "ok". The whole persistent-dataset
# design rests on a human remembering a constant; this is the reminder.
#
# Deliberately a diff check rather than a source hash: hashing the file would
# force a multi-minute reseed on every comment edit.
set -eu

SEED=crates/server/benches/hot_paths/seed.rs

if [ "${CI_PIPELINE_EVENT:-}" = "pull_request" ] && [ -n "${CI_COMMIT_TARGET_BRANCH:-}" ]; then
  git fetch -q origin "$CI_COMMIT_TARGET_BRANCH" 2>/dev/null || true
  base=$(git merge-base FETCH_HEAD HEAD 2>/dev/null || true)
else
  base=${CI_PREV_COMMIT_SHA:-}
fi
base=${1:-$base}
head=${2:-HEAD}

if [ -z "$base" ] || ! git cat-file -e "$base^{commit}" 2>/dev/null; then
  echo "check-seed-version: no usable diff base; skipping"
  exit 0
fi

if git diff --quiet "$base" "$head" -- "$SEED"; then
  echo "check-seed-version: $SEED unchanged"
  exit 0
fi

if git diff "$base" "$head" -- "$SEED" | grep -q '^[-+]pub const SEED_VERSION'; then
  echo "check-seed-version: $SEED changed and SEED_VERSION was bumped"
  exit 0
fi

# `seed.rs` holds the run lifecycle as well as the dataset: `reset_run_residue`,
# `verify` and `assert_dataset_unchanged` all live there, and none of them
# produces a row. Bumping the version for one of those is not a no-op — it
# discards every existing `plamenu_bench` and regenerates the dataset, so the
# budgets need recalibrating and the committed records stop being comparable to
# anything. A change that genuinely cannot alter the seeded shape declares
# itself on one of the lines it touches; the token has to be in the diff, so it
# cannot be left behind to cover a later change.
if git diff "$base" "$head" -- "$SEED" | grep -q '^+.*SEED-SHAPE-UNCHANGED'; then
  echo "check-seed-version: $SEED changed, declared shape-neutral"
  exit 0
fi

echo "$SEED changed but SEED_VERSION did not." >&2
echo "Every existing plamenu_bench database would keep the old dataset, and the" >&2
echo "budgets would be graded against a shape the code no longer produces." >&2
echo "Bump SEED_VERSION in $SEED, or revert the dataset change." >&2
exit 1
