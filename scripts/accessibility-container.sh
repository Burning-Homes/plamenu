#!/usr/bin/env bash
# Only invoked inside a disposable browser container by the fixture runner.
set -euo pipefail
if [[ "${A11Y_DISPOSABLE_TRUST:-}" != 1 || ! -f /tmp/plamenu-fixture-ca.crt ]]; then
    echo 'Disposable browser fixture certificate is required' >&2
    exit 1
fi
cp /tmp/plamenu-fixture-ca.crt /usr/local/share/ca-certificates/plamenu-fixture.crt
update-ca-certificates >/dev/null
setpriv --reuid="$A11Y_RUN_UID" --regid="$A11Y_RUN_GID" --clear-groups \
    python3 "$(dirname -- "$0")/accessibility-nss-trust.py"
exec setpriv --reuid="$A11Y_RUN_UID" --regid="$A11Y_RUN_GID" --clear-groups "$@"
