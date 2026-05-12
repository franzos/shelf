//! Integration tests for sequence numbering.

use std::path::Path;

use chrono::{NaiveDate, NaiveDateTime};
use filetime::FileTime;
use rusqlite::params;
use shelf::config::{Profile, SequenceScope};
use shelf::sequence::Sequencer;
use shelf::state::{FileId, State};

/// Materialize a minimal profile via TOML — both `Profile` and `Sequence`
/// are `#[non_exhaustive]` outside the crate.
fn profile(scope: SequenceScope, start: u64) -> Profile {
    let scope_str = match scope {
        SequenceScope::Global => "global",
        SequenceScope::Year => "year",
        SequenceScope::Month => "month",
        SequenceScope::Day => "day",
        SequenceScope::Folder => "folder",
    };
    let toml_src = format!(
        r#"
inputs = ["/tmp/i"]

[sequence]
scope = "{scope_str}"
start = {start}

[[output]]
name = "lib"
path = "/tmp/o"
directory = "{{yyyy}}"
filename = "{{yyyy}}-{{mm}}-{{dd}}_{{seq:05}}"
"#
    );
    toml::from_str(&toml_src).expect("base profile parses")
}

fn dt(y: i32, m: u32, d: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
}

/// Insert a `files` row via raw SQL; sidesteps `Metadata` being
/// `#[non_exhaustive]`. The sequencer only cares about `id`.
fn insert_file(state: &State, tmp: &Path, name: &str, when: NaiveDateTime) -> FileId {
    let path = tmp.join(name);
    std::fs::write(&path, name.as_bytes()).expect("write fixture");
    filetime::set_file_mtime(&path, FileTime::from_unix_time(1_700_000_000, 0)).unwrap();

    let meta = std::fs::metadata(&path).unwrap();
    let size = i64::try_from(meta.len()).unwrap();
    let source_path = path.to_string_lossy().to_string();
    let taken_at = when.format("%Y-%m-%dT%H:%M:%S").to_string();
    let now = "2024-01-01T00:00:00.000Z";
    state
        .conn()
        .execute(
            "INSERT INTO files ( \
                source_path, size, mtime_secs, mtime_nanos, sha256, \
                taken_at, taken_at_source, kind, \
                first_seen, last_seen \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                source_path,
                size,
                1_700_000_000_i64,
                0_i64,
                "0".repeat(64),
                taken_at,
                "mtime",
                "photo",
                now,
                now,
            ],
        )
        .unwrap();
    FileId(state.conn().last_insert_rowid())
}

fn key_for(scope: SequenceScope, when: NaiveDateTime) -> String {
    Sequencer::scope_key(scope, &when, None)
}

#[test]
fn day_scope_resets_per_day() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut state = State::open_in_memory().unwrap();
    let prof = profile(SequenceScope::Day, 1);

    let day1 = dt(2024, 3, 15);
    let day2 = dt(2024, 3, 16);
    let f1 = insert_file(&state, tmp.path(), "a.jpg", day1);
    let f2 = insert_file(&state, tmp.path(), "b.jpg", day1);
    let f3 = insert_file(&state, tmp.path(), "c.jpg", day2);
    let f4 = insert_file(&state, tmp.path(), "d.jpg", day2);

    let mut seq = Sequencer::new(&mut state, &prof);
    let s1 = seq
        .assign(f1, "lib", &key_for(SequenceScope::Day, day1))
        .unwrap();
    let s2 = seq
        .assign(f2, "lib", &key_for(SequenceScope::Day, day1))
        .unwrap();
    let s3 = seq
        .assign(f3, "lib", &key_for(SequenceScope::Day, day2))
        .unwrap();
    let s4 = seq
        .assign(f4, "lib", &key_for(SequenceScope::Day, day2))
        .unwrap();

    assert_eq!((s1, s2), (1, 2), "day1 counter starts at start=1");
    assert_eq!((s3, s4), (1, 2), "day2 counter resets");
}

#[test]
fn rerun_returns_same_seq_for_same_file_output() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut state = State::open_in_memory().unwrap();
    let prof = profile(SequenceScope::Day, 1);

    let day = dt(2024, 3, 15);
    let f1 = insert_file(&state, tmp.path(), "a.jpg", day);
    let f2 = insert_file(&state, tmp.path(), "b.jpg", day);

    let k = key_for(SequenceScope::Day, day);
    let (s1, s2) = {
        let mut seq = Sequencer::new(&mut state, &prof);
        (
            seq.assign(f1, "lib", &k).unwrap(),
            seq.assign(f2, "lib", &k).unwrap(),
        )
    };

    let mut seq = Sequencer::new(&mut state, &prof);
    assert_eq!(seq.assign(f1, "lib", &k).unwrap(), s1);
    assert_eq!(seq.assign(f2, "lib", &k).unwrap(), s2);
}

#[test]
fn earlier_taken_at_added_after_gets_next_free_number() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut state = State::open_in_memory().unwrap();
    let prof = profile(SequenceScope::Day, 1);

    let day = dt(2024, 3, 15);
    let f1 = insert_file(&state, tmp.path(), "later1.jpg", day);
    let f2 = insert_file(&state, tmp.path(), "later2.jpg", day);

    let k = key_for(SequenceScope::Day, day);
    let (s1, s2) = {
        let mut seq = Sequencer::new(&mut state, &prof);
        (
            seq.assign(f1, "lib", &k).unwrap(),
            seq.assign(f2, "lib", &k).unwrap(),
        )
    };
    assert_eq!((s1, s2), (1, 2));

    let earlier = NaiveDate::from_ymd_opt(2024, 3, 15)
        .unwrap()
        .and_hms_opt(6, 0, 0)
        .unwrap();
    let f3 = insert_file(&state, tmp.path(), "earlier.jpg", earlier);

    let mut seq = Sequencer::new(&mut state, &prof);
    assert_eq!(seq.assign(f1, "lib", &k).unwrap(), s1);
    assert_eq!(seq.assign(f2, "lib", &k).unwrap(), s2);
    let s3 = seq.assign(f3, "lib", &k).unwrap();
    assert_eq!(s3, 3, "newcomer gets next free, no renumbering");
}

#[test]
fn deleted_placement_leaves_a_gap() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut state = State::open_in_memory().unwrap();
    let prof = profile(SequenceScope::Day, 1);

    let day = dt(2024, 3, 15);
    let f1 = insert_file(&state, tmp.path(), "a.jpg", day);
    let f2 = insert_file(&state, tmp.path(), "b.jpg", day);
    let f3 = insert_file(&state, tmp.path(), "c.jpg", day);

    let k = key_for(SequenceScope::Day, day);
    let (s1, s2, s3) = {
        let mut seq = Sequencer::new(&mut state, &prof);
        (
            seq.assign(f1, "lib", &k).unwrap(),
            seq.assign(f2, "lib", &k).unwrap(),
            seq.assign(f3, "lib", &k).unwrap(),
        )
    };
    assert_eq!((s1, s2, s3), (1, 2, 3));

    state
        .conn()
        .execute(
            "DELETE FROM placements WHERE file_id = ?1 AND output_name = 'lib'",
            params![f2.0],
        )
        .unwrap();

    let f4 = insert_file(&state, tmp.path(), "d.jpg", day);
    let mut seq = Sequencer::new(&mut state, &prof);
    let s4 = seq.assign(f4, "lib", &k).unwrap();
    assert_eq!(s4, 4, "gap preserved; no compaction");
}

#[test]
fn start_value_is_respected_when_bucket_is_empty() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut state = State::open_in_memory().unwrap();
    let prof = profile(SequenceScope::Day, 100);

    let day = dt(2024, 3, 15);
    let f1 = insert_file(&state, tmp.path(), "a.jpg", day);
    let k = key_for(SequenceScope::Day, day);

    let mut seq = Sequencer::new(&mut state, &prof);
    assert_eq!(seq.assign(f1, "lib", &k).unwrap(), 100);
}

#[test]
fn buckets_are_isolated_per_output() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut state = State::open_in_memory().unwrap();
    let prof = profile(SequenceScope::Day, 1);

    let day = dt(2024, 3, 15);
    let f1 = insert_file(&state, tmp.path(), "a.jpg", day);
    let f2 = insert_file(&state, tmp.path(), "b.jpg", day);
    let k = key_for(SequenceScope::Day, day);

    let mut seq = Sequencer::new(&mut state, &prof);
    assert_eq!(seq.assign(f1, "lib", &k).unwrap(), 1);
    assert_eq!(seq.assign(f1, "archive", &k).unwrap(), 1);
    assert_eq!(seq.assign(f2, "lib", &k).unwrap(), 2);
    assert_eq!(seq.assign(f2, "archive", &k).unwrap(), 2);
}

#[test]
fn folder_scope_uses_destination_directory_as_bucket() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut state = State::open_in_memory().unwrap();
    let prof = profile(SequenceScope::Folder, 1);

    let day = dt(2024, 3, 15);
    let f1 = insert_file(&state, tmp.path(), "a.jpg", day);
    let f2 = insert_file(&state, tmp.path(), "b.jpg", day);
    let f3 = insert_file(&state, tmp.path(), "c.jpg", day);

    let k_photos = Sequencer::scope_key(SequenceScope::Folder, &day, Some("2024/03"));
    let k_videos = Sequencer::scope_key(SequenceScope::Folder, &day, Some("2024/03/videos"));

    let mut seq = Sequencer::new(&mut state, &prof);
    assert_eq!(seq.assign(f1, "lib", &k_photos).unwrap(), 1);
    assert_eq!(seq.assign(f2, "lib", &k_photos).unwrap(), 2);
    assert_eq!(
        seq.assign(f3, "lib", &k_videos).unwrap(),
        1,
        "different folder bucket → counter restarts"
    );
}
