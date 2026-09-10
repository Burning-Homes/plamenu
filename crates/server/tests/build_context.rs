//! Guards the Docker build context.
//!
//! `Dockerfile` pulls the whole context in with `COPY . .`, and
//! local Docker builds use the repository root as their context.
//! `.dockerignore` is therefore the only
//! thing standing between local secrets — `plamenu.toml`, `plamenu.dev.toml`,
//! `.staging*.env`, `DEPLOY_RUNBOOK.md` — and the build context, its layer
//! cache, and any remote/shared builder. These files are deliberately excluded
//! from git, so nothing else would catch a regression. This test fails if any
//! of them stops being excluded, keeping `.dockerignore` at least as strict as
//! the security-relevant part of `.gitignore`.

use std::path::{Path, PathBuf};

/// Files that must never reach the Docker build context. Each is also excluded
/// from git, so this list mirrors the security-relevant part of `.gitignore`.
const MUST_EXCLUDE: &[&str] = &[
    "plamenu.toml",
    "plamenu.dev.toml",
    ".staging.env",
    ".staging.prod.env",
    ".env",
    "deploy/.env",
    "deploy/plamenu.toml",
    "DEPLOY_RUNBOOK.md",
    "image-digest.txt",
    "e2e/peers/mastodon-test/.env",
    "e2e/peers/mastodon-test/.credentials",
    "e2e/peers/mastodon-test/ca-bundle.crt",
    "e2e/peers/discourse-test/.admin-token",
    "e2e/peers/peertube-test/.sample.mp4",
    "e2e/peers/mastodon-test/storage/private-key.pem",
    "e2e/peers/discourse-source/config/database.yml",
];

fn context_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/crates/server; the build context is <root>.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("resolve repository root")
}

fn patterns(file: &Path) -> Vec<String> {
    let text =
        std::fs::read_to_string(file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with('!'))
        .map(ToOwned::to_owned)
        .collect()
}

/// Classic `*`-glob match (no `?`, no character classes — all we need here).
fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'*') {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Does any ignore `pattern` exclude the path `name` (or a parent directory of
/// it)? Handles the small dialect these files actually use: a leading `/`
/// anchor, a trailing `/` directory marker, a `**/` prefix, and `*` globs.
fn excluded_by(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|raw| {
        let pat = raw
            .trim_start_matches('/')
            .trim_end_matches('/')
            .trim_start_matches("**/");
        // Match the whole path, or any path segment prefix (a matched directory
        // excludes everything beneath it, e.g. `deploy/.env` under `deploy`).
        glob_match(pat, name)
            || name
                .match_indices('/')
                .any(|(i, _)| glob_match(pat, &name[..i]))
    })
}

#[test]
fn dockerignore_excludes_every_secret() {
    let root = context_root();
    let docker = patterns(&root.join(".dockerignore"));
    let git = patterns(&root.join(".gitignore"));

    for name in MUST_EXCLUDE {
        assert!(
            excluded_by(&docker, name),
            ".dockerignore must exclude the secret `{name}` from the Docker build \
             context; it would otherwise be swept in by `COPY . .`",
        );
        // Sanity: these are also kept out of git, so `.dockerignore` really is
        // mirroring `.gitignore` rather than diverging from it.
        assert!(
            excluded_by(&git, name),
            "`{name}` is expected to be git-ignored too; update MUST_EXCLUDE if the \
             secret inventory changed",
        );
    }
}

#[test]
fn glob_match_behaves() {
    assert!(glob_match(".staging*.env", ".staging.env"));
    assert!(glob_match(".staging*.env", ".staging.prod.env"));
    assert!(!glob_match(".staging*.env", ".staging.env.example"));
    assert!(glob_match("target", "target"));
    assert!(!glob_match("target", "target2"));
    assert!(glob_match("*.log", "server.log"));
}
