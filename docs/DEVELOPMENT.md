# Development

Install the pinned Rust toolchain, Docker Compose, PostgreSQL client tools,
FFmpeg/ffprobe, and cargo-nextest. E2E work also needs Python 3 and `uv`.
The development stack runs PostgreSQL 18 and Mailpit on loopback ports.

```sh
cp .env.example .env
./dev doctor
./dev up
```

`./dev up` generates an ignored configuration, creates a disposable local
account, and starts Plamenu in the background. Use `./dev run` for foreground
operation, and `./dev logs`, `./dev status`, and `./dev down` to manage it.
Set `PLAMENU_CONFIG` to use your own configuration.

Keep credentials, generated configuration, database dumps, media, and `.dev/`
out of version control. Media defaults to `.dev/media`; preserve that directory
when reusing a development database in another checkout.

## Choose a check

Run from the checkout; development commands load `.env` and export
`DATABASE_URL`. `./dev release` instead uses isolated configuration and disposable
services; see [Releasing](RELEASING.md).

| Command | Purpose |
| --- | --- |
| `./dev test` | Rust tests through cargo-nextest |
| `./dev check` | Formatting, Clippy, offline Cargo checks, and benchmark-budget unit tests when pytest is installed |
| `./dev sqlx` | Regenerate `.sqlx` query metadata after SQL or migration changes |
| `./dev sqlx-check` | Check query metadata without updating it |
| `./dev docs-check` | Build the application, check CLI help and documentation examples, and build/check the book |
| `./dev ci` | Local check, docs-check, test, and dependency policy when cargo-deny is installed |

`./dev test` prepares PostgreSQL's `template1` so SQLx tests can create isolated
schema copies. Use the disposable development database. Select a test suite
or filter when working on a specific feature:

```sh
./dev test -p plamenu --test web -E 'test(composer_)'
```

Commit generated `.sqlx` changes after running `./dev sqlx`; do not hand-edit
the metadata. For prose-only edits, check links and example syntax. See
[documentation guidance](contributing/index.md#documentation).

## Performance

For changes affecting database query counts or runtime cost, run the calibrated
benchmark and grade its results:

```sh
./dev bench
./dev bench-check --strict
```

Include the results with the change and commit the passing record in
`bench/results/`. Explain query-count changes, including decreases caused by
changed data. [Known query fan-outs](NPLUS1_INVENTORY.md) tracks remaining
per-item database work.

The benchmark uses the versioned `plamenu_bench` database and machine-specific
budgets. `./dev bench-concurrency` measures concurrent posting and delivery
fan-out. Calibration and coverage are described in `bench/README.md`.

## Regression and federation tests

Test the affected production entry point. Transport tests should exercise the
guarded HTTP client, including redirects. Mutations that enqueue federation
work need rollback coverage for both the data and the outgoing jobs. Deletion
tests should check the captured recipients. See [architecture](ARCHITECTURE.md).

For web changes, check the rendered controls and interactions in a browser;
server-response assertions cannot detect controls hidden by CSS or broken by
JavaScript.

For federation changes, check serialization and the relevant peer exchange.
The Python E2E suite is currently specific to one developer's machine and is
included for transparency and examples; it is not expected to work elsewhere
as provided. See [interoperability testing](INTEROPERABILITY.md) and
`e2e/README.md` for its setup limitations and existing workflow.
