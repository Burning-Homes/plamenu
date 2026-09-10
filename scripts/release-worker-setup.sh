#!/bin/sh
# Optional worker convenience; none of this setup is used by local releases.
set -eu
if command -v apk >/dev/null 2>&1; then
  echo 'Static checks require a Debian worker; use local static checks on this worker image.' >&2
  exit 1
fi
./scripts/ci/setup.sh verify
apt-get install -y -qq --no-install-recommends ffmpeg openssh-client python3-pytest python3-yaml
python3 scripts/ci/install-tools.py cargo-nextest mdbook cargo-deny
python3 -m venv --system-site-packages /tmp/plamenu-release-tools
/tmp/plamenu-release-tools/bin/pip install --quiet ruff==0.16.4
printf '%s\n' "$HOME/.cargo/bin" "$HOME/.local/share/plamenu-ci/bin" /tmp/plamenu-release-tools/bin >> "$GITHUB_PATH"
