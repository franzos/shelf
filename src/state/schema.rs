//! SQLite schema migrations.
//!
//! Append new migrations to [`migrations`]; never edit a shipped one.
//!
//! ## Timestamp columns
//!
//! Two ISO-8601 `TEXT` shapes are used. Readers must apply the matching
//! format to round-trip correctly:
//!
//! - **System timestamps (UTC, millisecond precision):**
//!   `files.first_seen`, `files.last_seen`, `placements.placed_at`,
//!   `health.detected_at`, `runs.started_at`, `runs.finished_at`,
//!   `runs.reverted_at`. Format `%Y-%m-%dT%H:%M:%S%.3fZ`, always UTC.
//!   The trailing `Z` is load-bearing.
//! - **Capture timestamps (naive-local, second precision):**
//!   `files.taken_at`. Format `%Y-%m-%dT%H:%M:%S`, **no timezone suffix**.
//!   Mirrors [`crate::metadata::Metadata::taken_at`]. Treating these as
//!   UTC would silently shift the timeline.

use rusqlite_migration::{M, Migrations};

/// Initial schema.
///
/// - `files.source_path` is `UNIQUE` so the cache lookup is an index probe.
/// - `mtime_secs` + `mtime_nanos` are split so the cache key has
///   nanosecond resolution without a float round-trip.
/// - `placements.UNIQUE(output_name, dest_path)` keeps the planner from
///   double-booking the same destination.
/// - `placements_by_dest_path` accelerates orphan detection (walk output
///   tree, probe by `dest_path`).
/// - `health.detected_at` is indexed for the "recent flags" query.
///
/// Index naming: `<table>_by_<column>`.
const M0001_INITIAL: &str = r"
CREATE TABLE files (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    source_path     TEXT    NOT NULL UNIQUE,
    size            INTEGER NOT NULL,
    mtime_secs      INTEGER NOT NULL,
    mtime_nanos     INTEGER NOT NULL,
    sha256          TEXT    NOT NULL,
    taken_at        TEXT    NOT NULL,
    taken_at_source TEXT    NOT NULL,
    kind            TEXT    NOT NULL,
    camera          TEXT,
    lens            TEXT,
    width           INTEGER,
    height          INTEGER,
    first_seen      TEXT    NOT NULL,
    last_seen       TEXT    NOT NULL
);

CREATE INDEX files_by_sha ON files(sha256);

CREATE TABLE placements (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id      INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    output_name  TEXT    NOT NULL,
    dest_path    TEXT    NOT NULL,
    seq          INTEGER,
    placed_at    TEXT    NOT NULL,
    UNIQUE(output_name, dest_path)
);

CREATE INDEX placements_by_file_id ON placements(file_id);
CREATE INDEX placements_by_dest_path ON placements(dest_path);

CREATE TABLE health (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id     INTEGER REFERENCES files(id) ON DELETE CASCADE,
    kind        TEXT    NOT NULL,
    detail      TEXT,
    detected_at TEXT    NOT NULL
);

CREATE INDEX health_by_detected_at ON health(detected_at);
";

/// Sequence numbering buckets.
///
/// `scope_key` on `placements` lets the sequencer find the next free
/// number within a bucket with a single indexed `MAX(seq)` probe. Key is
/// `""` for `Global`, `"YYYY"` for `Year`, `"YYYY-MM"` for `Month`,
/// `"YYYY-MM-DD"` for `Day`, the rendered destination dir for `Folder`.
///
/// Bucket lives on `placements` rather than a separate `sequences` table
/// because the placements row already carries `seq`; co-locating keeps
/// assignment to a single write.
///
/// Reservation rows use a synthetic `dest_path`
/// (`":reserved:<file_id>:<output_name>"`) so the seq is locked in before
/// the real destination has been resolved. The sentinel embeds `file_id`
/// and `output_name` so it never collides on `UNIQUE(output_name, dest_path)`.
const M0002_SEQUENCE_SCOPE: &str = r"
ALTER TABLE placements ADD COLUMN scope_key TEXT NOT NULL DEFAULT '';

CREATE INDEX placements_by_output_scope ON placements(output_name, scope_key);
";

/// PDF document metadata.
///
/// `vendor` (TODO.md invoices template) is a synonym for `author`; we
/// don't store it separately because the PDF extractor derives both from
/// the same `/Info /Author`, with `/Producer` as a fallback for vendor
/// only. Persisting vendor would duplicate that derivation. The fallback
/// chain runs at extraction time, the result lands in `author`, and
/// `{vendor}` reads it back from the same column.
///
/// Both columns are nullable: only document-shaped kinds populate them.
const M0003_PDF_METADATA: &str = r"
ALTER TABLE files ADD COLUMN author TEXT;
ALTER TABLE files ADD COLUMN title  TEXT;
";

/// Run history and revert.
///
/// `runs` tracks each `shelf run` and `shelf revert` (start/finish
/// timestamps, counts, dry-run flag, reverted-by linkage). `placements`
/// gains a nullable `run_id` plus `op_mode` so every placement carries
/// provenance. Both are nullable because pre-feature reservation rows
/// won't have them.
///
/// **Status semantics.** A run with `finished_at IS NULL` is "incomplete"
/// — the process died between `open_run` and `finish_run`.
///
/// **Revert linkage.** When `shelf revert <id>` completes, the target's
/// `reverted_at` and `reverted_by` get set. The revert row itself carries
/// `kind = 'revert'` and `target_run_id = <id>` so the relationship is
/// queryable from either end.
const M0004_RUNS: &str = r"
CREATE TABLE runs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at    TEXT    NOT NULL,
    finished_at   TEXT,
    profile       TEXT    NOT NULL,
    kind          TEXT    NOT NULL DEFAULT 'run',
    target_run_id INTEGER REFERENCES runs(id),
    from_paths    TEXT,
    dry_run       INTEGER NOT NULL DEFAULT 0,
    strict        INTEGER NOT NULL DEFAULT 0,
    placed        INTEGER NOT NULL DEFAULT 0,
    replaced      INTEGER NOT NULL DEFAULT 0,
    skipped_dup   INTEGER NOT NULL DEFAULT 0,
    skipped_conf  INTEGER NOT NULL DEFAULT 0,
    failed        INTEGER NOT NULL DEFAULT 0,
    health        INTEGER NOT NULL DEFAULT 0,
    reverted_at   TEXT,
    reverted_by   INTEGER REFERENCES runs(id)
);

CREATE INDEX runs_by_started ON runs(started_at);
CREATE INDEX runs_by_profile ON runs(profile);

ALTER TABLE placements ADD COLUMN run_id  INTEGER REFERENCES runs(id);
ALTER TABLE placements ADD COLUMN op_mode TEXT;
CREATE INDEX placements_by_run_id ON placements(run_id);
";

pub fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(M0001_INITIAL),
        M::up(M0002_SEQUENCE_SCOPE),
        M::up(M0003_PDF_METADATA),
        M::up(M0004_RUNS),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn sha256_hex(s: &str) -> String {
        let digest = Sha256::digest(s.as_bytes());
        let mut out = String::with_capacity(64);
        for b in digest {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }

    /// Pinned fingerprints of every shipped migration. Edits to a shipped
    /// migration silently corrupt existing DBs — a hash change forces a
    /// reviewer to add a new migration instead. Add a new entry below
    /// whenever a new migration ships; never edit an existing line.
    #[test]
    fn migration_fingerprints_are_pinned() {
        let pins: &[(&str, &str)] = &[
            (
                "M0001_INITIAL",
                "7ea1c271dbaf46f5961f84fbcb5963d062117fe7a02950ab91090d40117993d3",
            ),
            (
                "M0002_SEQUENCE_SCOPE",
                "9422d3ac83fd0388a02391293e9cb5ecb898df21aa5211b21d645cc0a4eb1411",
            ),
            (
                "M0003_PDF_METADATA",
                "d528031fd2ced8e3444f2daf658ea95c73c31a547e412e1a167d283d65c05dad",
            ),
            (
                "M0004_RUNS",
                "ce93e64b7e4b14717346815c889c13054ee93c43b3761423f2bed783e01a0b83",
            ),
        ];
        let sources: &[(&str, &str)] = &[
            ("M0001_INITIAL", M0001_INITIAL),
            ("M0002_SEQUENCE_SCOPE", M0002_SEQUENCE_SCOPE),
            ("M0003_PDF_METADATA", M0003_PDF_METADATA),
            ("M0004_RUNS", M0004_RUNS),
        ];
        assert_eq!(
            sources.len(),
            pins.len(),
            "add a new pin below when shipping a new migration"
        );
        for ((name, src), (pin_name, pin_hash)) in sources.iter().zip(pins.iter()) {
            assert_eq!(name, pin_name, "pin order must match source order");
            let actual = sha256_hex(src);
            assert_eq!(
                &actual, pin_hash,
                "migration `{name}` source changed — shipped migrations must \
                 not be edited. If this is intentional, ship a new migration \
                 instead and append a new pin."
            );
        }
    }
}
