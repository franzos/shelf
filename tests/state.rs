//! Integration tests for the state DB.
//!
//! Schema-shape checks live here because they only need a public connection
//! handle. Cache hit/miss tests live alongside [`State`] in
//! `src/state/mod.rs` — [`shelf::metadata::Metadata`] is `#[non_exhaustive]`
//! and so can only be constructed from inside the crate.

use std::collections::BTreeSet;
use std::path::Path;

use rusqlite::Connection;
use shelf::config::Profile;
use shelf::state::{HealthRow, State};
use tempfile::TempDir;

fn profile_with_db(db_path: &Path) -> Profile {
    let toml_src = format!(
        r#"
inputs = ["/tmp/i"]

[kinds]
photo = ["jpg"]

[metadata]
date_sources = ["mtime"]

[state]
database = "{}"

[[output]]
name = "lib"
path = "/tmp/out"
mode = "copy"
on_conflict = "rename"
directory = "{{yyyy}}/{{mm}}"
filename  = "{{yyyy}}-{{mm}}-{{dd}}_{{seq:05}}"
"#,
        db_path.display(),
    );
    toml::from_str(&toml_src).unwrap()
}

fn table_names(state: &State) -> BTreeSet<String> {
    let mut stmt = state
        .conn()
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .expect("prepare");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query");
    rows.map(|r| r.unwrap()).collect()
}

fn index_names(state: &State) -> BTreeSet<String> {
    let mut stmt = state
        .conn()
        .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%'")
        .expect("prepare");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query");
    rows.map(|r| r.unwrap()).collect()
}

#[test]
fn migrations_create_expected_tables() {
    let state = State::open_in_memory().expect("open");
    let tables = table_names(&state);
    assert!(tables.contains("files"), "missing files: {tables:?}");
    assert!(
        tables.contains("placements"),
        "missing placements: {tables:?}"
    );
    assert!(tables.contains("health"), "missing health: {tables:?}");
    assert!(tables.contains("runs"), "missing runs: {tables:?}");
}

#[test]
fn migrations_create_expected_indexes() {
    let state = State::open_in_memory().expect("open");
    let indexes = index_names(&state);
    for expected in [
        "files_by_sha",
        "placements_by_file_id",
        "placements_by_dest_path",
        "placements_by_output_scope",
        "placements_by_run_id",
        "health_by_detected_at",
        "runs_by_started",
        "runs_by_profile",
    ] {
        assert!(
            indexes.contains(expected),
            "missing index `{expected}`: {indexes:?}"
        );
    }
}

#[test]
fn foreign_keys_are_enforced() {
    let state = State::open_in_memory().expect("open");
    let enabled: i64 = state
        .conn()
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .expect("pragma");
    assert_eq!(enabled, 1, "foreign_keys should be ON");
}

#[test]
fn files_table_has_expected_columns() {
    let state = State::open_in_memory().expect("open");
    let mut stmt = state
        .conn()
        .prepare("PRAGMA table_info(files)")
        .expect("prepare");
    let cols: BTreeSet<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query")
        .map(|r| r.unwrap())
        .collect();

    for expected in [
        "id",
        "source_path",
        "size",
        "mtime_secs",
        "mtime_nanos",
        "sha256",
        "taken_at",
        "taken_at_source",
        "kind",
        "camera",
        "lens",
        "width",
        "height",
        "first_seen",
        "last_seen",
    ] {
        assert!(
            cols.contains(expected),
            "files table missing column `{expected}`: {cols:?}"
        );
    }
}

#[test]
fn placements_table_has_expected_columns() {
    let state = State::open_in_memory().expect("open");
    let mut stmt = state
        .conn()
        .prepare("PRAGMA table_info(placements)")
        .expect("prepare");
    let cols: BTreeSet<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query")
        .map(|r| r.unwrap())
        .collect();

    for expected in [
        "id",
        "file_id",
        "output_name",
        "dest_path",
        "seq",
        "placed_at",
        "scope_key",
        "run_id",
        "op_mode",
    ] {
        assert!(
            cols.contains(expected),
            "placements table missing column `{expected}`: {cols:?}"
        );
    }
}

#[test]
fn on_disk_db_uses_wal_journal_mode() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.db");
    let profile = profile_with_db(&db_path);
    let state = State::open("test", &profile).expect("open");

    let mode: String = state
        .conn()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .expect("pragma");
    assert_eq!(
        mode.to_ascii_lowercase(),
        "wal",
        "on-disk DBs must use WAL — required for concurrent readers and \
         a precondition for any future synchronous=NORMAL pragma change"
    );
}

#[test]
fn reopening_db_is_a_no_op_migration_and_preserves_data() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.db");
    let profile = profile_with_db(&db_path);

    {
        let mut state = State::open("test", &profile).expect("open");
        state
            .record_health(&HealthRow {
                kind: "drift",
                dest_path: "/some/path",
                detail: Some("seeded"),
            })
            .unwrap();
    }

    let state = State::open("test", &profile).expect("re-open");
    let (kind, detail): (String, Option<String>) = state
        .conn()
        .query_row("SELECT kind, detail FROM health LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .expect("data preserved");
    assert_eq!(kind, "drift");
    assert_eq!(detail.as_deref(), Some("seeded"));
}

/// A second `State` opened against the same on-disk DB while the first
/// holds no active transaction must succeed (sequential reopens are the
/// common case: `shelf run` finishes, `shelf verify` opens fresh).
#[test]
fn sequential_writers_against_same_db_both_commit() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.db");
    let profile = profile_with_db(&db_path);

    {
        let mut state = State::open("test", &profile).expect("open A");
        state
            .record_health(&HealthRow {
                kind: "drift",
                dest_path: "/a/1",
                detail: None,
            })
            .unwrap();
    }
    {
        let mut state = State::open("test", &profile).expect("open B");
        state
            .record_health(&HealthRow {
                kind: "missing-destination",
                dest_path: "/b/1",
                detail: None,
            })
            .unwrap();
    }

    let conn = Connection::open(&db_path).unwrap();
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM health", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 2);
}

/// A read-only connection opened while another connection is mid-write
/// must observe the pre-write state (WAL snapshot isolation). The
/// follow-on perf change `synchronous=NORMAL` does not weaken this — it
/// only changes durability on crash, not reader/writer isolation. This
/// test pins the isolation guarantee so a future pragma change can't
/// silently regress it.
#[test]
fn readonly_connection_sees_snapshot_during_writer_transaction() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.db");
    let profile = profile_with_db(&db_path);

    let mut writer = State::open("test", &profile).expect("open writer");
    writer
        .record_health(&HealthRow {
            kind: "drift",
            dest_path: "/baseline",
            detail: None,
        })
        .unwrap();

    let reader = Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("open reader");

    let tx = writer.conn().unchecked_transaction().unwrap();
    tx.execute(
        "INSERT INTO health (kind, detail, detected_at) \
         VALUES ('drift', NULL, '2024-01-01T00:00:00.000Z')",
        [],
    )
    .unwrap();

    let mid_count: i64 = reader
        .query_row("SELECT COUNT(*) FROM health", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        mid_count, 1,
        "reader must see the pre-transaction snapshot under WAL"
    );

    tx.commit().unwrap();
    let post_count: i64 = reader
        .query_row("SELECT COUNT(*) FROM health", [], |r| r.get(0))
        .unwrap();
    assert_eq!(post_count, 2, "reader sees commit on next query");
}

#[test]
fn runs_table_has_expected_columns() {
    let state = State::open_in_memory().expect("open");
    let mut stmt = state
        .conn()
        .prepare("PRAGMA table_info(runs)")
        .expect("prepare");
    let cols: BTreeSet<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query")
        .map(|r| r.unwrap())
        .collect();

    for expected in [
        "id",
        "started_at",
        "finished_at",
        "profile",
        "kind",
        "target_run_id",
        "from_paths",
        "dry_run",
        "strict",
        "placed",
        "replaced",
        "skipped_dup",
        "skipped_conf",
        "failed",
        "health",
        "reverted_at",
        "reverted_by",
    ] {
        assert!(
            cols.contains(expected),
            "runs table missing column `{expected}`: {cols:?}"
        );
    }
}
