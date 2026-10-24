#[path = "../benches/hot_paths/run_manifest.rs"]
mod run_manifest;

use std::path::Path;
use std::process::Command;

use run_manifest::{MANIFEST, RunManifest, output_directory};
use serde_json::{Value, json};

fn read(directory: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(directory.join(MANIFEST)).unwrap()).unwrap()
}

#[test]
fn each_attempt_supersedes_latest_and_preserves_completed_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    // Also retain the last manifest from the older harness verbatim.
    let previous = br#"{"started_at":"old","cases":{"old":{}}}"#;
    std::fs::write(root.join(MANIFEST), previous).unwrap();
    let first = RunManifest::begin(root, json!({"started_at": "first"})).unwrap();
    let pending = read(root);
    assert_eq!(pending["run_status"], "running");
    assert!(pending.get("finished_at").is_none());
    let id = pending["run_id"].as_str().unwrap();
    first
        .finish(json!({"finished_at": "done", "cases": {"api/one": {"queries": 3}}}))
        .unwrap();
    let completed = read(root);
    assert_eq!(completed["run_status"], "complete");
    assert_eq!(completed, read(&root.join("plamenu-runs").join(id)));
    assert!(
        std::fs::read_dir(root.join("plamenu-runs"))
            .unwrap()
            .any(|entry| {
                let path = entry.unwrap().path();
                path.is_file() && std::fs::read(path).unwrap() == previous
            })
    );
    let second = RunManifest::begin(root, json!({"started_at": "second"})).unwrap();
    assert_ne!(read(root)["run_id"], completed["run_id"]);
    assert_eq!(read(root)["run_status"], "running");
    drop(second);
    assert_eq!(read(root)["run_status"], "running");
    assert_eq!(completed, read(&root.join("plamenu-runs").join(id)));
}

#[test]
fn completion_write_failure_keeps_latest_nonpassing() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let run = RunManifest::begin(root, json!({})).unwrap();
    let pending = read(root);
    std::fs::remove_dir_all(
        root.join("plamenu-runs")
            .join(pending["run_id"].as_str().unwrap()),
    )
    .unwrap();
    assert!(run.finish(json!({"finished_at": "done"})).is_err());
    assert_eq!(read(root), pending);
}

#[test]
fn failed_start_invalidates_the_previous_latest_manifest() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::write(root.join(MANIFEST), br#"{"run_status":"complete"}"#).unwrap();
    std::fs::write(root.join("plamenu-runs"), "not a directory").unwrap();
    assert!(RunManifest::begin(root, json!({})).is_err());
    assert!(!root.join(MANIFEST).exists());
}

#[test]
fn another_attempt_cannot_be_finalized_by_an_older_writer() {
    let directory = tempfile::tempdir().unwrap();
    let first = RunManifest::begin(directory.path(), json!({})).unwrap();
    let second = RunManifest::begin(directory.path(), json!({})).unwrap();
    let pending = read(directory.path());
    assert!(first.finish(json!({"finished_at": "done"})).is_err());
    assert_eq!(read(directory.path()), pending);
    second.finish(json!({"finished_at": "done"})).unwrap();
    assert_eq!(read(directory.path())["run_status"], "complete");
}

#[test]
fn abrupt_exit_leaves_a_nonpassing_boundary() {
    const CHILD: &str = "PLAMENU_BOUNDARY_CHILD";
    if let Some(directory) = std::env::var_os(CHILD) {
        let _run = RunManifest::begin(Path::new(&directory), json!({})).unwrap();
        // Deliberately bypass Drop and all normal finalization, like a killed
        // setup process; no fixtures or measurements are needed.
        std::process::exit(101);
    }
    let directory = tempfile::tempdir().unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "abrupt_exit_leaves_a_nonpassing_boundary",
            "--test-threads=1",
        ])
        .env(CHILD, directory.path())
        .output()
        .unwrap()
        .status;
    assert_eq!(status.code(), Some(101));
    assert_eq!(read(directory.path())["run_status"], "running");
}

#[test]
fn criterion_home_and_target_precedence_bind_both_outputs() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::write(root.join("Cargo.lock"), "").unwrap();
    let package = root.join("crates/server");
    for (home, target, expected) in [
        (
            Some("custom"),
            Some("other"),
            std::path::PathBuf::from("custom"),
        ),
        (
            None,
            Some("other"),
            std::path::PathBuf::from("other/criterion"),
        ),
        (None, None, root.join("target/criterion")),
    ] {
        assert_eq!(
            output_directory(home.map(Into::into), target.map(Into::into), &package),
            expected
        );
    }
}
