#!/bin/sh
# Debian worker setup. Rust and downloaded executables are version/checksum pinned.
set -eu
mode=${1:?quick, clippy or verify}
case "$mode" in quick|clippy|verify) ;; *) exit 2 ;; esac
apt-get update -qq
apt-get install -y -qq --no-install-recommends ca-certificates curl git build-essential \
  pkg-config perl python3 python3-yaml python3-venv shellcheck time zstd
case "$(uname -m)" in
  x86_64) triple=x86_64-unknown-linux-gnu; digest=20a06e644b0d9bd2fbdbfd52d42540bdde820ea7df86e92e533c073da0cdd43c ;;
  aarch64) triple=aarch64-unknown-linux-gnu; digest=e3853c5a252fca15252d07cb23a1bdd9377a8c6f3efa01531109281ae47f841c ;;
  *) echo 'Unsupported worker architecture' >&2; exit 1 ;;
esac
export PATH="$HOME/.cargo/bin:$PATH"
if ! command -v rustup >/dev/null 2>&1; then
  curl --fail --silent --show-error --location --retry 3 \
    "https://static.rust-lang.org/rustup/archive/1.28.2/$triple/rustup-init" -o /tmp/rustup-init
  printf '%s  %s\n' "$digest" /tmp/rustup-init | sha256sum -c -
  chmod +x /tmp/rustup-init
  /tmp/rustup-init -y --no-modify-path --default-toolchain none
fi
toolchain=$(python3 -c 'import tomllib; print(tomllib.load(open("rust-toolchain.toml", "rb"))["toolchain"]["channel"])')
rustup toolchain install "$toolchain" --profile minimal --component rustfmt --component clippy
printf '%s\n' "$HOME/.cargo/bin" >> "$GITHUB_PATH"
rustc --version
cargo clippy --version
if [ "$mode" = quick ]; then
  python3 scripts/ci/install-tools.py cargo-deny
  printf '%s\n' "$HOME/.local/share/plamenu-ci/bin" >> "$GITHUB_PATH"
  /usr/bin/python3 -m venv --system-site-packages /tmp/plamenu-ci-tools
  /tmp/plamenu-ci-tools/bin/pip -q install ruff==0.16.4
  printf '%s\n' /tmp/plamenu-ci-tools/bin >> "$GITHUB_PATH"
  test "$triple" = x86_64-unknown-linux-gnu
  curl --fail --silent --show-error --location --retry 3 \
    https://github.com/gitleaks/gitleaks/releases/download/v8.27.2/gitleaks_8.27.2_linux_x64.tar.gz \
    -o /tmp/gitleaks.tar.gz
  printf '%s  %s\n' 141c3b2dede46d8b3a53b47116da756bd223decc0374797559a6b50ecba5590c /tmp/gitleaks.tar.gz | sha256sum -c -
  tar -xzf /tmp/gitleaks.tar.gz -C /tmp/plamenu-ci-tools/bin gitleaks
fi
