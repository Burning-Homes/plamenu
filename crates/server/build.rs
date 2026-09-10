use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    for name in [
        "CI_COMMIT_SHA",
        "CI_PIPELINE_NUMBER",
        "PLAMENU_BUILD_CHANNEL",
        "PLAMENU_BUILD_NUMBER",
        "PLAMENU_GIT_DIRTY",
        "PLAMENU_GIT_SHA",
    ] {
        println!("cargo::rerun-if-env-changed={name}");
    }

    let root = workspace_root();
    track_repository_inputs(&root);

    let package_version = env::var("CARGO_PKG_VERSION").expect("Cargo sets CARGO_PKG_VERSION");
    let target = env::var("TARGET").expect("Cargo sets TARGET");
    configure_linker(&target);
    let profile = env::var("PROFILE").expect("Cargo sets PROFILE");
    let debug_assertions = env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some();
    let channel = env::var("PLAMENU_BUILD_CHANNEL").unwrap_or_else(|_| "development".to_owned());
    assert!(
        matches!(channel.as_str(), "release" | "staging" | "development"),
        "PLAMENU_BUILD_CHANNEL must be release, staging, or development"
    );
    assert!(
        channel != "release" || !debug_assertions,
        "a release-channel build cannot enable debug assertions"
    );

    let build_number = first_nonempty_env(&["PLAMENU_BUILD_NUMBER", "CI_PIPELINE_NUMBER"])
        .or_else(|| git(&root, &["rev-list", "--count", "HEAD"]));
    assert!(
        channel != "release" || build_number.is_some(),
        "a release-channel build requires PLAMENU_BUILD_NUMBER"
    );
    let build_id = build_number.as_deref().unwrap_or("local");
    validate_identifier("build number", build_id, true);

    let revision = first_nonempty_env(&["PLAMENU_GIT_SHA", "CI_COMMIT_SHA"])
        .or_else(|| git(&root, &["rev-parse", "HEAD"]));
    assert!(
        channel != "release" || revision.is_some(),
        "a release-channel build requires PLAMENU_GIT_SHA"
    );
    let revision = revision.unwrap_or_else(|| "unknown".to_owned());
    validate_identifier("Git revision", &revision, false);
    let short_revision: String = revision.chars().take(12).collect();

    let dirty = env::var("PLAMENU_GIT_DIRTY")
        .map_or_else(|_| repository_is_dirty(&root), |value| parse_bool(&value));
    assert!(
        channel != "release" || !dirty,
        "a release-channel build cannot use a dirty source tree"
    );

    let public_version = if channel == "release" {
        package_version.clone()
    } else {
        prerelease_version(
            &package_version,
            &channel,
            build_id,
            &short_revision,
            debug_assertions,
            dirty,
        )
    };
    let full_version = if channel == "release" {
        release_build_version(&package_version, build_id, &short_revision)
    } else {
        public_version.clone()
    };

    rustc_env("PLAMENU_BUILD_CHANNEL_RESOLVED", &channel);
    rustc_env(
        "PLAMENU_BUILD_NUMBER_RESOLVED",
        build_number.as_deref().unwrap_or(""),
    );
    println!("cargo::rustc-check-cfg=cfg(plamenu_build_number)");
    if build_number.is_some() {
        println!("cargo::rustc-cfg=plamenu_build_number");
    }
    rustc_env("PLAMENU_BUILD_PROFILE", &profile);
    rustc_env("PLAMENU_BUILD_TARGET", &target);
    rustc_env("PLAMENU_FULL_VERSION", &full_version);
    rustc_env("PLAMENU_GIT_REVISION", &revision);
    rustc_env("PLAMENU_GIT_REVISION_SHORT", &short_revision);
    rustc_env("PLAMENU_PUBLIC_VERSION", &public_version);
    if dirty {
        rustc_env("PLAMENU_BUILD_DIRTY", "1");
    }
    println!(
        "cargo::rustc-check-cfg=cfg(plamenu_channel, values(\"release\", \"staging\", \"development\"))"
    );
    println!("cargo::rustc-cfg=plamenu_channel=\"{channel}\"");
}

fn configure_linker(target: &str) {
    // The shipped static-musl PIE has tens of thousands of relative
    // relocations. DT_RELR stores them compactly without changing PIE/ASLR
    // behaviour. Keep this restricted to the two GNU-ld musl targets used by
    // our release builders; other targets may use linkers without RELR support.
    if matches!(
        target,
        "x86_64-unknown-linux-musl" | "aarch64-unknown-linux-musl"
    ) {
        println!("cargo::rustc-link-arg=-Wl,-z,pack-relative-relocs");
    }
}

fn workspace_root() -> PathBuf {
    Path::new(&env::var("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR")).join("../..")
}

fn first_nonempty_env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| env::var(name).ok().filter(|value| !value.is_empty()))
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn repository_is_dirty(root: &Path) -> bool {
    git(root, &["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|status| !status.is_empty())
}

fn parse_bool(value: &str) -> bool {
    match value {
        "1" | "true" | "yes" => true,
        "0" | "false" | "no" => false,
        _ => panic!("PLAMENU_GIT_DIRTY must be true or false"),
    }
}

fn track_repository_inputs(root: &Path) {
    for path in [
        "Cargo.toml",
        "crates/ap/Cargo.toml",
        "crates/ap/src",
        "crates/db/Cargo.toml",
        "crates/db/migrations",
        "crates/db/src",
        "crates/federation/Cargo.toml",
        "crates/federation/src",
        "crates/server/Cargo.toml",
        "crates/server/src",
    ] {
        println!("cargo::rerun-if-changed={}", root.join(path).display());
    }
    for path in [".git/HEAD", ".git/index"] {
        let path = root.join(path);
        if path.exists() {
            println!("cargo::rerun-if-changed={}", path.display());
        }
    }
    if let Ok(head) = std::fs::read_to_string(root.join(".git/HEAD"))
        && let Some(reference) = head.trim().strip_prefix("ref: ")
    {
        let path = root.join(".git").join(reference);
        if path.exists() {
            println!("cargo::rerun-if-changed={}", path.display());
        }
    }
}

fn validate_identifier(name: &str, value: &str, forbid_numeric_leading_zero: bool) {
    assert!(
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && !(forbid_numeric_leading_zero
                && value.len() > 1
                && value.starts_with('0')
                && value.bytes().all(|byte| byte.is_ascii_digit())),
        "{name} must be a valid SemVer identifier without numeric leading zeroes: {value:?}"
    );
}

fn split_metadata(version: &str) -> (&str, Option<&str>) {
    version
        .split_once('+')
        .map_or((version, None), |(core, metadata)| (core, Some(metadata)))
}

fn prerelease_version(
    version: &str,
    channel: &str,
    build: &str,
    revision: &str,
    debug_assertions: bool,
    dirty: bool,
) -> String {
    let (core, existing_metadata) = split_metadata(version);
    let intent = match (channel, debug_assertions) {
        ("staging", true) => "staging.debug",
        ("staging", false) => "staging",
        ("development", true) => "dev.debug",
        ("development", false) => "dev",
        _ => unreachable!("channel was validated"),
    };
    let separator = if core.contains('-') { '.' } else { '-' };
    let mut result = format!("{core}{separator}{intent}.{build}");
    let mut metadata = existing_metadata.map_or_else(String::new, |value| format!("{value}."));
    metadata.push_str("sha.");
    metadata.push_str(revision);
    if dirty {
        metadata.push_str(".dirty");
    }
    result.push('+');
    result.push_str(&metadata);
    result
}

fn release_build_version(version: &str, build: &str, revision: &str) -> String {
    let (core, existing_metadata) = split_metadata(version);
    let metadata = existing_metadata.map_or_else(String::new, |value| format!("{value}."));
    format!("{core}+{metadata}build.{build}.sha.{revision}")
}

fn rustc_env(name: &str, value: &str) {
    println!("cargo::rustc-env={name}={value}");
}
