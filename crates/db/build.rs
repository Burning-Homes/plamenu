//! Rebuild this crate when a migration is added.
//!
//! `sqlx::migrate!()` expands to an `include_str!` per migration file, so cargo
//! already knows to rebuild when an existing one is *edited*. A **new** file is
//! named by no existing `include_str!`, so nothing invalidates the crate and the
//! compiled `MIGRATOR` silently keeps the old list: the added migration never
//! runs, `connect_and_migrate` reports success, and the schema the binary
//! believes in is not the schema on disk. Caught when migration 0034 did not
//! reach the benchmark database and a run measured the un-indexed plans while
//! reporting the migration as applied.
fn main() {
    println!("cargo::rerun-if-changed=migrations");
}
