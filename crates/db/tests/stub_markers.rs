//! Anti-sprawl guard for the thread-stub discipline: every SELECT/WITH whose
//! SQL reads the `statuses` table must state its stance on soft-deleted stubs
//! with a marker comment — `-- STUBFILTER` (the read excludes stubs) or
//! `-- STUBKEEP: <why>` (the read deliberately sees them). A new query without
//! a marker fails here, so the decision is made in review instead of being
//! discovered later as a placeholder leaking into a timeline (that is not
//! hypothetical: this guard's initial sweep found the trends, link-timeline
//! and outbox pages serving stubs).
//!
//! Writes (`DELETE`/`INSERT`/`UPDATE`) are exempt: they target rows by
//! identity and display nothing. The scan covers this crate and
//! `crates/server/src`, which is where every other sqlx query in the
//! workspace lives.

use std::path::{Path, PathBuf};

/// Extracts every string literal (plain `"…"`, raw `r"…"` and `r#"…"#`) from
/// Rust source. Good enough for our SQL literals: none embed an unescaped
/// quote inside a bare raw string.
#[allow(
    clippy::naive_bytecount,
    reason = "a test scanning a few source files; not worth a bytecount dependency"
)]
fn string_literals(src: &str) -> Vec<(usize, String)> {
    // Byte-oriented so multi-byte characters in the source (doc comments use
    // '…' and '→' freely) never land a slice on a non-boundary.
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let line_of = |pos: usize| bytes[..pos].iter().filter(|b| **b == b'\n').count() + 1;
    let find = |haystack: &[u8], needle: &[u8]| -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    };
    while i < bytes.len() {
        if bytes[i..].starts_with(b"r#\"") {
            let Some(end) = find(&bytes[i + 3..], b"\"#") else {
                break;
            };
            let text = String::from_utf8_lossy(&bytes[i + 3..i + 3 + end]).into_owned();
            out.push((line_of(i), text));
            i += 3 + end + 2;
        } else if bytes[i..].starts_with(b"r\"") {
            let Some(end) = find(&bytes[i + 2..], b"\"") else {
                break;
            };
            let text = String::from_utf8_lossy(&bytes[i + 2..i + 2 + end]).into_owned();
            out.push((line_of(i), text));
            i += 2 + end + 1;
        } else if bytes[i] == b'"' {
            let mut j = i + 1;
            let mut buf: Vec<u8> = Vec::new();
            while j < bytes.len() {
                match bytes[j] {
                    b'\\' => {
                        buf.push(b'.');
                        j += 2;
                    }
                    b'"' => break,
                    b => {
                        buf.push(b);
                        j += 1;
                    }
                }
            }
            out.push((line_of(i), String::from_utf8_lossy(&buf).into_owned()));
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Whether the literal reads the `statuses` table (`FROM statuses` or
/// `JOIN statuses`, on a word boundary — `status_tags` etc. don't count).
fn reads_statuses(sql: &str) -> bool {
    for keyword in ["FROM statuses", "JOIN statuses"] {
        let mut start = 0;
        while let Some(pos) = sql[start..].find(keyword) {
            let after = start + pos + keyword.len();
            let boundary = sql[after..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_');
            if boundary {
                return true;
            }
            start = after;
        }
    }
    false
}

fn scan_dir(dir: &Path, violations: &mut Vec<String>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            scan_dir(&path, violations);
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        for (line, lit) in string_literals(&src) {
            if !reads_statuses(&lit) {
                continue;
            }
            let Some(first) = lit.split_whitespace().next() else {
                continue;
            };
            match first.to_ascii_uppercase().as_str() {
                "SELECT" | "WITH" => {}
                _ => continue, // writes and non-SQL strings
            }
            if !lit.contains("STUBFILTER") && !lit.contains("STUBKEEP") {
                violations.push(format!(
                    "{}:{line}: statuses read without a STUBFILTER/STUBKEEP marker",
                    path.display()
                ));
            }
        }
    }
}

#[test]
fn every_statuses_read_states_its_stub_stance() {
    let db_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let server_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../server/src");
    let mut violations = Vec::new();
    scan_dir(&db_src, &mut violations);
    scan_dir(&server_src, &mut violations);
    assert!(
        violations.is_empty(),
        "every SELECT/WITH reading `statuses` must carry `-- STUBFILTER` or \
         `-- STUBKEEP: <why>` (does this read want soft-deleted stubs?):\n{}",
        violations.join("\n")
    );
}

/// The scanner itself must keep seeing what it guards — if the literal
/// extractor or the table matcher rot, the guard silently passes on
/// everything, so pin them on known shapes.
#[test]
fn the_scanner_recognises_the_shapes_it_guards() {
    let raw = r##"let q = sqlx::query!(r#"SELECT id FROM statuses WHERE id = $1"#);"##;
    let lits = string_literals(raw);
    assert_eq!(lits.len(), 1);
    assert!(reads_statuses(&lits[0].1));
    assert!(reads_statuses(
        "WITH x AS (SELECT 1) SELECT * FROM x JOIN statuses s ON TRUE"
    ));
    assert!(!reads_statuses("SELECT 1 FROM status_tags"));
    assert!(!reads_statuses("SELECT 1 FROM statuses_archive"));
}
