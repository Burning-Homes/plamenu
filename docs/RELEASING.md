# Release procedure

Prepare and test a release locally, then publish those same bytes:

```sh
./dev release
./dev release --publish
```

The second command reuses verified completed stages. It can also be used alone
to prepare and publish in one invocation. No CI run IDs, artifact downloads,
handwritten approval files, or separate promotion step are needed.

## Before the first run

Every release includes **Linux AMD64 and ARM64** installation archives and a
container image for both architectures. Run preparation on Linux AMD64 or
ARM64. It needs:

- Python 3.11 or newer, Git, OpenSSH client tools, the pinned Rust toolchain
  (including rustfmt and Clippy), a native C build toolchain, pkg-config, and Perl;
- Docker with Compose and Buildx, usable without sudo;
- FFmpeg/ffprobe and binutils (`readelf`);
- cargo-nextest, cargo-sqlx matching the workspace's SQLx version, cargo-deny,
  mdbook, ShellCheck, Ruff, and the Python `pytest` and `PyYAML` packages.

The pinned binary installer can install cargo-nextest, cargo-deny, mdbook, and
Cosign: `python3 scripts/ci/install-tools.py`. Add
`~/.local/share/plamenu-ci/bin` to `PATH`. Cosign is needed only for publication;
Git SSH signing must also be configured for that step.

```sh
./dev release --plan
```

This lists the selected stages, missing command/package prerequisites, and
missing publication or worker settings when those operations are requested.
Actual runs also check Docker access, Compose, Buildx, and execution of each
locally built architecture. Allow sufficient disk
space for a separate checkout, Cargo output, Docker caches, and release files.
Native source checks reuse a dedicated Cargo cache across release revisions.
The default build uses two compilation jobs and preserves Fat LTO; it can need
several GiB of RAM. `--jobs N` changes compilation concurrency. No CPU model, fixed database port, standing development services, or Codefloe
account is required for local preparation. Internet access is needed for dependencies and
container images that are not already cached.

For both architectures on one machine, Docker needs emulation for the other
architecture. Docker Desktop normally includes it; on Linux Docker Engine,
install the missing emulator once. For example, on AMD64:

```sh
docker run --privileged --rm \
  tonistiigi/binfmt@sha256:400a4873b838d1b89194d982c45e5fb3cda4593fbfd7e08a02e76b03b21166f0 \
  --install arm64
```

Use `--install amd64` on ARM64. The command registers QEMU with the Docker
host's kernel; it needs a host that permits that operation. Emulated compilation
is slower than native compilation. The release command tests execution before
starting lengthy checks and fails clearly if a local target is unavailable.
See [Docker's multi-platform build documentation](https://docs.docker.com/build/building/multi-platform/).
An architecture sent to a native worker does not require local emulation.

Set `workspace.package.version`, update the workspace entries in `Cargo.lock`,
and move the changelog's `Unreleased` entry into a dated version section.
Commit the release source. Preparation requires a clean checkout and captures
that exact commit in a separate worktree; later work in the original checkout
does not change the running release.

## What preparation checks

The command runs formatting, Clippy, script and budget tests, dependency policy,
static checks, documentation checks, Rust application tests, and SQLx metadata
validation. Database checks use a disposable PostgreSQL 18 container bound to a
random loopback port, with an empty template database for migration tests.
SQLx validation compares fresh query metadata while respecting explicit `!`/`?`
nullability annotations, so query-plan differences on an empty database do not
produce false failures. It does not rewrite the committed `.sqlx` cache. The
normal release path does not load `.env`.

For each architecture, it builds one static musl executable, packages the
installation files, and assembles a runtime image containing that executable.
The image's binary hash and ELF architecture must match the installation
archive. Each image then passes the installation, backup/restore, and federation
drill on its build machine, using emulation when necessary. When changing host
installation or backup procedures, also exercise both architecture archives on
fresh Debian hosts, including a real reboot and restore into empty storage.
After publication, check anonymous downloads and image pulls; a deployed server
also needs public DNS, TLS, and readiness checks for its actual domain.

Outputs and logs live under `release-artifacts/vVERSION/INPUT-ID/`.
The `assets/` directory contains:

| File | Contents |
| --- | --- |
| `plamenu-VERSION-linux-amd64.tar.gz` | One Linux AMD64 executable, systemd unit, Plamenu and Caddy examples, license |
| `plamenu-VERSION-linux-arm64.tar.gz` | The equivalent installation files for Linux ARM64 |
| `plamenu-VERSION-source.tar.gz` | Source from the exact release commit |
| `release.json` | Source/build settings, check outcomes, and report hashes |
| `SHA256SUMS` | Checksums for the prepared downloads |

There is no second executable or diagnostic archive. Tested container images
remain in `build-amd64/image.tar.gz` and `build-arm64/image.tar.gz`; load one
locally with `docker load --input PATH`. These local transport files are not
release downloads. Publishing uploads the same images and combines their
immutable digests under one multi-architecture registry tag. It does not
recompile either executable or rebuild either runtime image.

The release page automatically lists the installation downloads, describes
what is inside them, and includes a direct container-package link, pull command,
immutable image reference, and verification links. Publication also links the
container package to the source repository and enables its Packages tab when
both use the same forge owner.

Rerunning the same command verifies saved file hashes and resumes after a
failure. Static checks are refreshed after 24 hours to catch changed dependency
advisories until publication starts; publication retries retain the same check
results and signed files. Use `--rerun` to repeat all stages, or `--check-only` to run source
checks without packaging. Changed source or compilation options select a separate output directory.
Changing an architecture's build location reruns that architecture only; other
completed stages remain reusable. Do not run simultaneous publishers for the same version.
An existing published version is immutable: new build bytes or check results
require a new version rather than replacement uploads.

## Optional developer checks

```sh
./dev release --skip-e2e
```

`--skip-e2e` explicitly omits the Python federation suite; omission is also the
default. **Skipping it is appropriate for someone building on another machine:**
the suite relies on one developer's ad hoc peer fleet, credentials, URLs, and
running services. It is not portable or expected to work elsewhere as provided;
it is included for transparency and examples. Requiring that setup would test
someone else's environment rather than provide a usable build prerequisite.
The normal application and packaged-image restore checks still run, including
the drill's Plamenu-to-Plamenu federation checks. Skipping the fleet does not
establish interoperability with the third-party software it covers.

Maintainers with the existing fleet can opt in with `--with-e2e`. This uses their
original checkout's `.env` and peer workspace, and captures the suite report.
It tests the configured live fleet; the separate restore drill tests the newly
built release image. See [Interoperability testing](INTEROPERABILITY.md).

Timing benchmarks are the **other machine-specific check**. Use
`--with-performance` only on the calibration machine in
`bench/release-policy.toml`. The command runs and strictly grades fresh hot-path
and contention reports automatically; there is no requirement to commit or
supply those reports. Different CPUs cannot meaningfully pass the same timing
thresholds without recalibration. Benchmark-budget unit tests remain part of
the portable baseline. Both optional checks appear as `not-run`, with a reason,
when omitted; omission is never reported as a pass.

## Publishing setup

Create `~/.config/plamenu/release.toml` once (or select another file with
`--config`). For example:

```toml
repository = "https://codefloe.com/plamenu/plamenu"
registry = "codefloe.com/plamenu/plamenu"
git_remote = "codefloe"
username = "YOUR_ACCOUNT"
token_file = "~/.config/plamenu/release-token"
signing_key = "~/.config/plamenu/cosign.key"
# Optional overrides; defaults are the reviewed keys in the release source:
# public_key = "/path/to/cosign.pub"
# allowed_signers = "/path/to/allowed-signers"
```

Keep the token and encrypted signing key outside Git, with owner-only file
permissions. The token needs repository release access and package read/write
access; optional workers also need Actions access. Git uses the configured
remote's normal authentication. Supply the encrypted key password through
`COSIGN_PASSWORD` in the publishing environment. Credentials are not passed to
the compiler or the optional workers.

Publication checks repository and package-owner privacy by default; private
images must use that forge owner's registry namespace. After an
explicit decision to publish publicly, set `allow_public = true` in this local
configuration. Configure the registry namespace's visibility deliberately;
repository and package visibility are separate. The registry host receives the
configured token, so use a token intended for that host.

`--publish` verifies the signing keys and source tag, pushes both tested images,
checks each pull by immutable digest, and publishes one index containing exactly
`linux/amd64` and `linux/arm64`. It signs that index and the checksum manifest, and
creates/verifies/pushes the normal SSH-signed `vVERSION` tag. It uploads release
assets into a draft, reads every asset back, verifies them, and finalizes the
draft last. It adds `image.txt`, `cosign.pub`, and `SHA256SUMS.bundle` to the
published assets. It does not move `latest` or `stable`.

Platform images also have `vVERSION-amd64` and `vVERSION-arm64` tags; normal
users pull `vVERSION` and their runtime selects the correct architecture.

Failed publication can leave an image, tag, or draft. Rerun the same command to
finish matching uploads; conflicting tags, images, or assets are refused.
The final URL and immutable image reference are saved in `publication/` beside
the prepared stages. Signing and publication remain local even when a worker
builds the release.

Obtain `release/cosign.pub` and `release/allowed-signers` through reviewed signed
Git history. Cosign uses explicit keys without public transparency-log
submissions. Verify downloaded assets with the independently trusted key:

```sh
cosign verify --key release/cosign.pub --insecure-ignore-tlog \
  codefloe.com/plamenu/plamenu@sha256:REPLACE_WITH_RELEASE_DIGEST
cosign verify-blob --key release/cosign.pub --insecure-ignore-tlog \
  --bundle SHA256SUMS.bundle SHA256SUMS
sha256sum -c SHA256SUMS
```

Private images require registry login with package-read access. Keep protected
backups of signing keys outside Git. Rotate keys through reviewed signed
changes and retain old public keys for verifying older releases.

## Optional workers

```sh
./dev release --arm64-on codefloe
./dev release --arm64-on codefloe --publish
# Optionally offload AMD64 or static checks too:
./dev release --amd64-on codefloe --arm64-on codefloe --checks-on codefloe --publish
```

The default builds both architectures locally. `--arm64-on codefloe` sends the
ARM64 build and image drill to a native ARM64 worker; AMD64 stays local.
These options require the configured repository and token, plus a pushed branch
or tag at the selected commit containing `release-worker.yml`. The command
dispatches the selected stage, waits, retrieves its result, checks its source and
file hashes, and continues locally. There are no manual workflow inputs or
artifact-retention chores. A retry reconnects to an unfinished request. Once
retrieved, results are ordinary durable local files; forge artifact expiry no
longer matters. If a result expired before download, retry to dispatch again or
omit the worker option to run locally.

`--checks-on` offloads static checks only. Application/database checks, optional
developer checks, signing, and publication stay local. Each image's restore
checks run where that image is built, using the same implementation.
Workers are a convenience, and local preparation needs no release config.
See [CI/CD](CI-CD.md) for workflow details and validation limits.

## Documentation and build identity

Build a local preview with `./dev docs-build VERSION`; the checked site is in
`target/docs/site/VERSION/`. Every page shows its Plamenu version, release or
development status, build number, and source revision. **Development** means
that source is not tagged as that release; **local changes** marks a dirty
preview. `build.json` contains the full identity and generated-file hashes.

### Codefloe Pages

The [documentation site](https://docs.plamenu.codefloe.page/) is hosted on
Codefloe Pages from the separate `plamenu/docs` repository. For a new or
recreated docs repository, enable Pages once in **Settings → Pages**, selecting
`main`. That repository holds only generated HTML, styles, and search files;
`docs-deploy` maintains it automatically. Edit documentation in the application
source repository.
The site is public even while the source repository remains private. Codefloe
serves the generated files through statichost.eu; no hosted compiler or Actions
workflow is needed. See [Codefloe's Pages configuration](https://docs.codefloe.com/pages/configuration/).

Add this to the same local release configuration used for publication:

```toml
[docs]
enabled = true
repository = "https://codefloe.com/plamenu/docs"
url = "https://docs.plamenu.codefloe.page/"
branch = "main"
```

`docs.repository` selects the generated-site repository on the same forge;
its SSH push URL is derived automatically, with no extra source remote needed.
Docs deployment reuses `token_file`, `allow_public`, and Git signing settings.
Without `docs.repository`, the existing `repository` and `git_remote` are used;
in that case use a separate deployment branch such as `pages`. It needs local Git, Python 3.11+
and mdBook, but no application build, database, Docker, or Cosign key.

```sh
./dev docs-deploy                     # Publish the current committed source
./dev docs-deploy --ref v0.6.0         # Publish or retry a particular release
./dev release --publish               # Also deploy docs when enabled above
```

`--config PATH` selects a different local configuration. A standalone deployment
captures the selected commit in a temporary worktree. Release publication uses
its already captured source and runs docs deployment only after the signed
release is available. Local release preparation does not require Pages.

The command builds and checks links locally, signs a commit containing only the
generated site, and pushes it to the configured docs repository and branch. A normal Git push rejects concurrent
updates. It then reads the public build marker and checks every hosted file
against the local hashes, without sending forge credentials to the site.
The local record and log are in `target/docs/deploy/SOURCE/`; the release hook
keeps them under the release's `docs/` directory.

If hosting fails, the release remains published and the command reports
**release published; docs pending**. Retry `docs-deploy --ref SOURCE` with the
same configuration; unchanged output reuses the existing signed commit and
checks hosting again. No binaries are rebuilt and no release tag or assets are
changed. Hosted verification waits up to five minutes; use `--timeout SECONDS`
when retrying a slower deployment. Never force-push over a concurrent publisher;
check the current site and deliberately select the source to deploy next.

The release version comes from Cargo, the revision from the captured commit,
and the release build number from that commit's timestamp. Local and worker
release builds use the same identity without depending on a forge run number.
Other development builds retain their existing Git-count fallback. The admin
dashboard shows the full build identity, target, and architecture.
