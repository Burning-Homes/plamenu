# CI/CD

Plamenu uses Forgejo Actions on Codefloe for contribution checks and optional,
manually requested release workers. Release preparation, testing, signing, and
publication can all run locally.

## Contribution checks

[checks.yml](https://codefloe.com/plamenu/plamenu/src/branch/main/.forgejo/workflows/checks.yml) runs on pull request changes,
pushes to `main`, and manual dispatch. New runs cancel older checks for the
same branch or pull request.

| Job | Checks | Timeout |
| --- | --- | --- |
| `quick` | Rust formatting, shell lint, YAML/TOML/Python syntax, Python lint and formatting, dependency policy, secret scanning, public-tree rules, DCO trailers, and seed-version changes | 5 minutes |
| `clippy` | `cargo clippy --locked --workspace --all-targets -- -D warnings` with `SQLX_OFFLINE=true` | 10 minutes |

Clippy waits for `quick` to pass, avoiding compilation when a static check
already fails. Both use AMD64 workers and two Cargo jobs. Clippy checks test
and benchmark targets without executing them; committed SQLx metadata removes
the need for a database. Contribution jobs receive no release or deployment
secrets, and checkout does not persist credentials.

Tool versions and Actions revisions are pinned. Rust tools and Clippy output
are cached by toolchain, with the compilation cache also keyed by `Cargo.lock`.
Only non-PR runs on `main` save caches. The compilation cache includes compressed
dependency downloads and is saved only below a 1 GiB uncompressed limit.
Installing `zstd` before restore and save keeps the cache format consistent.

The repository protection configuration in
[ci/codefloe-settings.json](https://codefloe.com/plamenu/plamenu/src/branch/main/ci/codefloe-settings.json) requires both PR
statuses before merging into `main`, including for administrators. Direct and
force pushes are disabled. The `maintainers` team controls `v*` tags. DCO
trailers and cryptographic signatures are separate checks: CI checks trailers;
authored commits must also be signed and their signatures verified. New-branch
pushes and rewritten root commits check the complete history. A root commit introduces the benchmark
seed; later dataset changes still require a seed-version bump. Manual checks
also work when the selected commit has no parent.

Secret scanning retains the standard detectors. `.gitleaks.toml` excludes only
reviewed fixture values in specific files, so exclusions survive rewritten
history without hiding new credentials in those files.

## Local verification and releases

`./dev release` owns the release pipeline. It runs locally by default: source
checks, disposable database tests, compilation and packaging, and an
install/restore drill against the resulting image. `./dev release --publish`
adds local signing and publication of those same bytes. See the
[release procedure](RELEASING.md) for prerequisites and one-time configuration.

The Python federation fleet is developer-specific and omitted by default;
`--skip-e2e` makes that explicit. Calibrated timing benchmarks are also optional
because their thresholds depend on the designated machine. Application tests,
SQLx checks, documentation checks, budget unit tests, and the container restore
drill do not depend on that developer setup. Automatic contribution checks
alone do not establish that a release was tested.

## Optional release workers

[release-worker.yml](https://codefloe.com/plamenu/plamenu/src/branch/main/.forgejo/workflows/release-worker.yml)
is manually dispatched by `./dev release --arm64-on codefloe` or
`./dev release --checks-on codefloe`. Pushes, pull requests, tags, and schedules
do not start release jobs. The client supplies the source/settings, reconnects
to pending requests, downloads results, and validates the returned source and
hashes. Operators do not copy API IDs, manifests, or evidence files.

| Stage | Worker | Result | Timeout |
| --- | --- | --- | --- |
| Build and restore | Docker, native AMD64 or ARM64 | Installation archive, source archive, tested runtime image, metadata | 90 minutes |
| Static checks | Debian Trixie, AMD64 | Same static checks used locally, with logs | 90 minutes |

Both stages execute the same Python implementation as local preparation.
The optional BuildKit container has a two-CPU quota, 6 GiB RAM limit, and a
12 GiB memory-plus-swap allowance. This leaves room within the observed 7 GiB
job memory limit for the worker coordinator. The latter depends on existing worker swap;
it does not allocate swap. The GNU-hosted Rust toolchain targets musl with
Fat LTO and one codegen unit. These worker limits are not local-machine
requirements. Every release targets Linux AMD64 and ARM64; local builds can use QEMU and
workers must execute natively on the requested architecture.

The worker uploads one automatically retrieved bundle with a requested
three-day retention period. Once downloaded, its local copy has no forge
expiry dependency. Missing, expired, or mismatched results stop that stage;
repeating it locally is supported. Worker jobs have no signing or registry
credentials, do not publish, and do not use contribution compilation caches.
Release jobs are serialized and never automatically cancelled by newer jobs.

## Credentials and trust

Local preparation requires no forge credentials. Optional workers need a token
for the selected repository's Actions API. Local publication needs repository
and package access, an encrypted Cosign key, and a configured Git SSH signer.
The [release configuration](RELEASING.md#publishing-setup) names those files;
private keys remain on the publishing machine. Contribution jobs receive none
of these credentials. Repository and package-owner privacy are checked before
private uploads; public destinations require explicit local configuration.

Release trust is pinned in `release/cosign.pub` and `release/allowed-signers`.
The signed manifest binds the release files and automatic check summary; the
signed version tag identifies the source. Assets are read back and verified
before the release draft is finalized. Known conflicting versions are refused.

## Deployment

Deployment is a separate manual operation with environment-specific access.
`./dev deploy-staging` builds and deploys to the configured staging environment,
checks health, and retains the previous binary for rollback. It is independent
of Codefloe CI. `./dev docs-deploy` builds and checks locally, pushes a signed
commit of generated content to the separate `plamenu/docs` repository, and
verifies the public site. When docs
hosting is enabled in local release configuration, `./dev release --publish`
uses the same operation after publication, from the captured release source.
A hosting failure can be retried independently. See
[Pages deployment](RELEASING.md#codefloe-pages).
