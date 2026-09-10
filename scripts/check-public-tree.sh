#!/bin/sh
set -eu

required='README.md LICENSE CONTRIBUTING.md SECURITY.md SUPPORT.md CHANGELOG.md Dockerfile dev compose.dev.yml deploy/compose.yml scripts/deploy-staging.sh'
for path in $required; do
  test -s "$path" || { echo "missing required public file: $path" >&2; exit 1; }
done

for forbidden in AGENTS.md deploy-staging.sh CLAUDE.md .claude; do
  test -z "$(git ls-files -- "$forbidden" "$forbidden/**")" || {
    echo "internal-only path is tracked: $forbidden" >&2
    exit 1
  }
done

if git grep -nE 'example\.invalid|plam-staging-01|root@sdu\.li|/home/v/Projects' -- \
  ':!scripts/check-public-tree.sh'; then
  echo "internal or placeholder repository reference found" >&2
  exit 1
fi

if grep -nE '\[NOTE:|\[Author name and contact\]|<project-domain>' \
  SECURITY.md; then
  echo "public document template placeholder found" >&2
  exit 1
fi

if git grep -niE 'CODE_OF_CONDUCT|CODE OF CONDUCT|codeberg\.org/plamenu' -- \
  ':(exclude)scripts/check-public-tree.sh'; then
  echo "retired policy or repository reference found" >&2
  exit 1
fi

test -z "$(git status --porcelain --untracked-files=no)" || {
  echo "tracked files changed during validation" >&2
  exit 1
}
