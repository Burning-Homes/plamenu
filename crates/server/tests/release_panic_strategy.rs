//! Guards the release panic strategy.
//!
//! The background-worker supervisor (`supervise_with` in
//! `crates/server/src/main.rs`) recovers a panicked worker by observing its
//! erroring `JoinHandle` and respawning it, and an unwinding request-handler
//! panic drops a single connection rather than the whole server. Both depend on
//! panics *unwinding*. `panic = "abort"` in the release profile would abort the
//! process on any panic instead, silently defeating every supervised restart —
//! and the supervisor's own unit test would keep passing, because the test and
//! bench profiles always unwind regardless of the release setting.
//!
//! A `#[cfg(panic = "abort")] compile_error!` beside the supervisor already
//! fails any real (release) build that selects abort. This test is its twin in
//! the ordinary `cargo nextest` gate: it fails the moment the workspace manifest
//! reintroduces `panic = "abort"`, before a release build is even attempted.

use std::path::{Path, PathBuf};

fn workspace_manifest() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/crates/server; the workspace root is <root>.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../Cargo.toml")
        .canonicalize()
        .expect("resolve workspace Cargo.toml")
}

/// The `panic = "..."` value declared in a given `[profile.<name>]` table, or
/// `None` if the table or the key is absent. A minimal hand-parser (no `toml`
/// dependency, matching `build_context.rs`) over the small dialect this manifest
/// uses: `[section]` headers on their own line and `key = "value"` entries.
fn profile_panic(manifest: &str, profile: &str) -> Option<String> {
    let header = format!("[profile.{profile}]");
    let mut in_section = false;
    for line in manifest.lines() {
        // Strip trailing comments, then trim.
        let code = line.split('#').next().unwrap_or("").trim();
        if code.is_empty() {
            continue;
        }
        if code.starts_with('[') && code.ends_with(']') {
            in_section = code == header;
            continue;
        }
        if in_section
            && let Some((key, value)) = code.split_once('=')
            && key.trim() == "panic"
        {
            return Some(value.trim().trim_matches('"').to_owned());
        }
    }
    None
}

#[test]
fn release_profile_does_not_abort_on_panic() {
    let manifest = std::fs::read_to_string(workspace_manifest()).expect("read workspace manifest");
    let panic = profile_panic(&manifest, "release");

    // The correctness floor: abort defeats the worker supervisor.
    // Removing the key entirely would still unwind (the profile default), so the
    // real requirement is "not abort" rather than "present".
    assert_ne!(
        panic.as_deref(),
        Some("abort"),
        "the release profile must not set panic = \"abort\": it would abort the whole \
         process on any worker/handler panic and defeat the supervisor",
    );

    // We deliberately pin it explicitly to `unwind` and document why in Cargo.toml,
    // so a reader sees the requirement at the profile rather than inferring the
    // default. Keep the manifest self-consistent with that comment.
    assert_eq!(
        panic.as_deref(),
        Some("unwind"),
        "the release profile is expected to pin panic = \"unwind\" explicitly \
         #19); update this test intentionally if the pinning strategy changes",
    );
}

#[test]
fn profile_panic_parses_the_dialect() {
    let sample = "\
[profile.dev]
opt-level = 0

[profile.release]
lto = \"thin\"
panic = \"unwind\"  # required by the supervisor
strip = \"debuginfo\"

[profile.bench]
inherits = \"release\"
";
    assert_eq!(profile_panic(sample, "release").as_deref(), Some("unwind"));
    assert_eq!(profile_panic(sample, "dev"), None);
    assert_eq!(profile_panic(sample, "bench"), None);
    assert_eq!(profile_panic("panic = \"abort\"\n", "release"), None);
    assert_eq!(
        profile_panic("[profile.release]\npanic = \"abort\"\n", "release").as_deref(),
        Some("abort"),
    );
}
