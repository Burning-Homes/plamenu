//! Related account tests share one default binary; individual targets remain selectable.

#[path = "account_collections.rs"]
mod account_collections;
#[path = "account_feed.rs"]
mod account_feed;
#[path = "applications_settings.rs"]
mod applications_settings;
#[path = "local_identity.rs"]
mod local_identity;

/// Turning off a default target must never silently remove its source cases.
#[test]
fn default_harnesses_cover_every_suppressed_integration_target_once() {
    use std::collections::HashMap;
    use std::path::PathBuf;

    let package = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tests = package.join("tests").canonicalize().unwrap();
    let manifest: toml::Value = toml::from_str(include_str!("../Cargo.toml")).unwrap();
    assert_ne!(
        manifest["package"]
            .get("autotests")
            .and_then(toml::Value::as_bool),
        Some(false),
        "New integration files must remain automatically discoverable"
    );
    let mut suppressed = HashMap::new();
    for target in manifest
        .get("test")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        if target.get("test").and_then(toml::Value::as_bool) != Some(false) {
            continue;
        }
        let name = target["name"].as_str().unwrap();
        let path = target
            .get("path")
            .and_then(toml::Value::as_str)
            .map_or_else(
                || package.join(format!("tests/{name}.rs")),
                |path| package.join(path),
            )
            .canonicalize()
            .unwrap();
        assert_eq!(path.parent(), Some(tests.as_path()));
        assert!(suppressed.insert(name.to_owned(), path).is_none());
    }
    let modules =
        regex::Regex::new(r#"#\[\s*path\s*=\s*"([^"]+)"\s*\]\s*mod\s+([a-zA-Z_]\w*)\s*;"#).unwrap();
    let mut inclusions = HashMap::<String, usize>::new();
    for entry in std::fs::read_dir(&tests).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|value| value.to_str()) != Some("rs") {
            continue;
        }
        let name = path.file_stem().unwrap().to_str().unwrap();
        if suppressed.contains_key(name) {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        for module in modules.captures_iter(&source) {
            let included = tests.join(&module[1]).canonicalize().unwrap();
            if included.parent() != Some(tests.as_path()) {
                continue;
            }
            let original = included.file_stem().unwrap().to_str().unwrap();
            assert_eq!(
                suppressed.get(original),
                Some(&included),
                "{} includes a root test file still enabled as an individual default target",
                path.display()
            );
            assert_eq!(
                &module[2], original,
                "Keep the original target name in grouped case identities"
            );
            *inclusions.entry(original.to_owned()).or_default() += 1;
        }
    }
    for original in suppressed.keys() {
        assert_eq!(
            inclusions.get(original),
            Some(&1),
            "Suppressed target {original} must appear in exactly one default harness"
        );
    }
}
