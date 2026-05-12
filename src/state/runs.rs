//! Run history rows and revert linkage.
//!
//! A `run` row is opened at the start of every `shelf run` and `shelf revert`
//! invocation, then finalised on completion. Mid-run crashes leave the row
//! with `finished_at IS NULL` so `shelf runs` flags it as `(incomplete)`.
//!
//! `runs.from_paths` is a JSON array of strings (or NULL). We hand-roll
//! encode/decode rather than pull in `serde_json` for one flat list of
//! paths. The decoder is forgiving: on malformed input it returns an
//! empty list since `from_paths` is advisory display data.

use std::path::PathBuf;

use rusqlite::{OptionalExtension, params};

use super::{State, now_iso, sqlite_err_with};
use crate::error::{Error, Result};

/// Foreign key for `runs.id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunId(pub i64);

/// Aggregated outcome of a `shelf run` (or `shelf revert`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RunCounts {
    pub placed: u64,
    pub replaced: u64,
    pub skipped_duplicate: u64,
    pub skipped_conflict: u64,
    pub failed: u64,
    pub health: u64,
}

/// One row from the `runs` table, decoded for display.
#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: RunId,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub profile: String,
    pub kind: String,
    pub target_run_id: Option<RunId>,
    pub from_paths: Vec<String>,
    pub dry_run: bool,
    pub strict: bool,
    pub counts: RunCounts,
    pub reverted_at: Option<String>,
    pub reverted_by: Option<RunId>,
}

/// One placement row joined with its source path.
#[derive(Debug, Clone)]
pub struct PlacementRow {
    pub file_id: i64,
    pub source_path: PathBuf,
    pub dest_path: PathBuf,
    pub output_name: String,
    pub seq: Option<i64>,
    pub scope_key: String,
    pub placed_at: String,
    pub op_mode: Option<String>,
    pub sha256: String,
}

impl State {
    /// Open a fresh `runs` row for a `shelf run`. `finished_at` stays
    /// NULL until [`State::finish_run`] succeeds.
    pub fn open_run(
        &mut self,
        profile: &str,
        from_paths: &[PathBuf],
        dry_run: bool,
        strict: bool,
    ) -> Result<RunId> {
        let db_path = self.db_path.clone();
        let sql = sqlite_err_with(&db_path);
        let now = now_iso();
        let from_json = if from_paths.is_empty() {
            None
        } else {
            Some(encode_from_paths(from_paths))
        };
        self.conn
            .execute(
                "INSERT INTO runs ( \
                    started_at, profile, kind, from_paths, dry_run, strict \
                 ) VALUES (?1, ?2, 'run', ?3, ?4, ?5)",
                params![
                    &now,
                    profile,
                    from_json,
                    i64::from(dry_run),
                    i64::from(strict),
                ],
            )
            .map_err(&sql)?;
        Ok(RunId(self.conn.last_insert_rowid()))
    }

    /// Open a fresh `runs` row for a `shelf revert <target_run_id>`.
    pub fn open_revert_run(&mut self, profile: &str, target_run_id: RunId) -> Result<RunId> {
        let db_path = self.db_path.clone();
        let sql = sqlite_err_with(&db_path);
        let now = now_iso();
        self.conn
            .execute(
                "INSERT INTO runs ( \
                    started_at, profile, kind, target_run_id \
                 ) VALUES (?1, ?2, 'revert', ?3)",
                params![&now, profile, target_run_id.0],
            )
            .map_err(&sql)?;
        Ok(RunId(self.conn.last_insert_rowid()))
    }

    /// Finalise a `runs` row with counts and `finished_at = now`.
    pub fn finish_run(&mut self, run_id: RunId, counts: &RunCounts) -> Result<()> {
        let db_path = self.db_path.clone();
        let sql = sqlite_err_with(&db_path);
        let now = now_iso();
        self.conn
            .execute(
                "UPDATE runs SET \
                    finished_at = ?1, \
                    placed = ?2, replaced = ?3, \
                    skipped_dup = ?4, skipped_conf = ?5, \
                    failed = ?6, health = ?7 \
                 WHERE id = ?8",
                params![
                    &now,
                    counts.placed as i64,
                    counts.replaced as i64,
                    counts.skipped_duplicate as i64,
                    counts.skipped_conflict as i64,
                    counts.failed as i64,
                    counts.health as i64,
                    run_id.0,
                ],
            )
            .map_err(&sql)?;
        Ok(())
    }

    /// Finalise a revert run row and stamp the original target as reverted.
    /// Both writes happen in one transaction so an observer never sees
    /// half the linkage. `--force` repeat-reverts are valid (the caller
    /// decides whether to allow them).
    pub fn finish_revert_run(
        &mut self,
        run_id: RunId,
        target_run_id: RunId,
        counts: &RunCounts,
    ) -> Result<()> {
        let db_path = self.db_path.clone();
        let sql = sqlite_err_with(&db_path);
        let now = now_iso();
        let tx = self.conn.transaction().map_err(&sql)?;
        tx.execute(
            "UPDATE runs SET \
                finished_at = ?1, \
                placed = ?2, replaced = ?3, \
                skipped_dup = ?4, skipped_conf = ?5, \
                failed = ?6, health = ?7 \
             WHERE id = ?8",
            params![
                &now,
                counts.placed as i64,
                counts.replaced as i64,
                counts.skipped_duplicate as i64,
                counts.skipped_conflict as i64,
                counts.failed as i64,
                counts.health as i64,
                run_id.0,
            ],
        )
        .map_err(&sql)?;
        tx.execute(
            "UPDATE runs SET reverted_at = ?1, reverted_by = ?2 WHERE id = ?3",
            params![&now, run_id.0, target_run_id.0],
        )
        .map_err(&sql)?;
        tx.commit().map_err(&sql)?;
        Ok(())
    }

    /// List runs, newest first.
    pub fn list_runs(&self, profile: Option<&str>, limit: usize) -> Result<Vec<RunRow>> {
        let db_path = self.db_path.clone();
        let map_sql = |e: rusqlite::Error| Error::Sqlite {
            path: db_path.clone(),
            source: e,
        };
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut rows = Vec::new();
        if let Some(p) = profile {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT id, started_at, finished_at, profile, kind, target_run_id, \
                            from_paths, dry_run, strict, placed, replaced, \
                            skipped_dup, skipped_conf, failed, health, \
                            reverted_at, reverted_by \
                     FROM runs WHERE profile = ?1 \
                     ORDER BY id DESC LIMIT ?2",
                )
                .map_err(map_sql)?;
            let mapped = stmt
                .query_map(params![p, limit_i64], row_to_runrow)
                .map_err(map_sql)?;
            for r in mapped {
                rows.push(r.map_err(map_sql)?);
            }
        } else {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT id, started_at, finished_at, profile, kind, target_run_id, \
                            from_paths, dry_run, strict, placed, replaced, \
                            skipped_dup, skipped_conf, failed, health, \
                            reverted_at, reverted_by \
                     FROM runs ORDER BY id DESC LIMIT ?1",
                )
                .map_err(map_sql)?;
            let mapped = stmt
                .query_map(params![limit_i64], row_to_runrow)
                .map_err(map_sql)?;
            for r in mapped {
                rows.push(r.map_err(map_sql)?);
            }
        }
        Ok(rows)
    }

    pub fn get_run(&self, run_id: RunId) -> Result<Option<RunRow>> {
        let db_path = self.db_path.clone();
        let map_sql = |e: rusqlite::Error| Error::Sqlite {
            path: db_path.clone(),
            source: e,
        };
        let row = self
            .conn
            .query_row(
                "SELECT id, started_at, finished_at, profile, kind, target_run_id, \
                        from_paths, dry_run, strict, placed, replaced, \
                        skipped_dup, skipped_conf, failed, health, \
                        reverted_at, reverted_by \
                 FROM runs WHERE id = ?1",
                params![run_id.0],
                row_to_runrow,
            )
            .optional()
            .map_err(map_sql)?;
        Ok(row)
    }

    /// All non-reserved placements created by a given run, joined with the
    /// source `files` row so the revert planner has source-path and sha256
    /// without a second query.
    pub fn placements_for_run(&self, run_id: RunId) -> Result<Vec<PlacementRow>> {
        let db_path = self.db_path.clone();
        let map_sql = |e: rusqlite::Error| Error::Sqlite {
            path: db_path.clone(),
            source: e,
        };
        let mut stmt = self
            .conn
            .prepare(
                "SELECT p.file_id, f.source_path, p.dest_path, p.output_name, \
                        p.seq, p.scope_key, p.placed_at, p.op_mode, f.sha256 \
                 FROM placements p JOIN files f ON p.file_id = f.id \
                 WHERE p.run_id = ?1 AND p.dest_path NOT LIKE ':reserved:%' \
                 ORDER BY p.id ASC",
            )
            .map_err(map_sql)?;
        let mapped = stmt
            .query_map(params![run_id.0], |row| {
                Ok(PlacementRow {
                    file_id: row.get(0)?,
                    source_path: PathBuf::from(row.get::<_, String>(1)?),
                    dest_path: PathBuf::from(row.get::<_, String>(2)?),
                    output_name: row.get(3)?,
                    seq: row.get(4)?,
                    scope_key: row.get(5)?,
                    placed_at: row.get(6)?,
                    op_mode: row.get(7)?,
                    sha256: row.get(8)?,
                })
            })
            .map_err(map_sql)?;
        let mut out = Vec::new();
        for r in mapped {
            out.push(r.map_err(map_sql)?);
        }
        Ok(out)
    }

    /// Delete the placements row for `(run_id, dest_path)`. Used by revert
    /// after the filesystem op succeeds.
    pub fn delete_placement_for_run(&mut self, run_id: RunId, dest_path: &str) -> Result<()> {
        let db_path = self.db_path.clone();
        let sql = sqlite_err_with(&db_path);
        self.conn
            .execute(
                "DELETE FROM placements WHERE run_id = ?1 AND dest_path = ?2",
                params![run_id.0, dest_path],
            )
            .map_err(&sql)?;
        Ok(())
    }
}

#[allow(clippy::cast_sign_loss)]
fn row_to_runrow(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    let id: i64 = row.get(0)?;
    let started_at: String = row.get(1)?;
    let finished_at: Option<String> = row.get(2)?;
    let profile: String = row.get(3)?;
    let kind: String = row.get(4)?;
    let target_run_id: Option<i64> = row.get(5)?;
    let from_paths_json: Option<String> = row.get(6)?;
    let dry_run_i: i64 = row.get(7)?;
    let strict_i: i64 = row.get(8)?;
    let placed: i64 = row.get(9)?;
    let replaced: i64 = row.get(10)?;
    let skipped_dup: i64 = row.get(11)?;
    let skipped_conf: i64 = row.get(12)?;
    let failed: i64 = row.get(13)?;
    let health: i64 = row.get(14)?;
    let reverted_at: Option<String> = row.get(15)?;
    let reverted_by: Option<i64> = row.get(16)?;

    Ok(RunRow {
        id: RunId(id),
        started_at,
        finished_at,
        profile,
        kind,
        target_run_id: target_run_id.map(RunId),
        from_paths: from_paths_json
            .map(|s| decode_from_paths(&s))
            .unwrap_or_default(),
        dry_run: dry_run_i != 0,
        strict: strict_i != 0,
        counts: RunCounts {
            placed: u64::try_from(placed.max(0)).unwrap_or(0),
            replaced: u64::try_from(replaced.max(0)).unwrap_or(0),
            skipped_duplicate: u64::try_from(skipped_dup.max(0)).unwrap_or(0),
            skipped_conflict: u64::try_from(skipped_conf.max(0)).unwrap_or(0),
            failed: u64::try_from(failed.max(0)).unwrap_or(0),
            health: u64::try_from(health.max(0)).unwrap_or(0),
        },
        reverted_at,
        reverted_by: reverted_by.map(RunId),
    })
}

/// Encode a slice of paths as a JSON string array. Escapes `\\`, `"`, and
/// C0 controls.
pub fn encode_from_paths(paths: &[PathBuf]) -> String {
    let mut out = String::from("[");
    for (i, p) in paths.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        write_escaped(&mut out, &p.to_string_lossy());
        out.push('"');
    }
    out.push(']');
    out
}

fn write_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

/// Best-effort JSON-array-of-strings decoder. Returns an empty list on
/// any shape we don't recognise — value is advisory display data.
pub fn decode_from_paths(s: &str) -> Vec<String> {
    let trimmed = s.trim();
    let Some(inner) = trimmed.strip_prefix('[').and_then(|r| r.strip_suffix(']')) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut chars = inner.chars().peekable();
    loop {
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() || c == ',' {
                chars.next();
            } else {
                break;
            }
        }
        match chars.peek() {
            None => break,
            Some(&'"') => {
                chars.next();
                let mut buf = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            let Some(esc) = chars.next() else { break };
                            match esc {
                                '"' => buf.push('"'),
                                '\\' => buf.push('\\'),
                                '/' => buf.push('/'),
                                'n' => buf.push('\n'),
                                'r' => buf.push('\r'),
                                't' => buf.push('\t'),
                                'b' => buf.push('\u{08}'),
                                'f' => buf.push('\u{0c}'),
                                'u' => {
                                    let mut hex = String::with_capacity(4);
                                    for _ in 0..4 {
                                        if let Some(h) = chars.next() {
                                            hex.push(h);
                                        }
                                    }
                                    if let Ok(n) = u32::from_str_radix(&hex, 16)
                                        && let Some(c) = char::from_u32(n)
                                    {
                                        buf.push(c);
                                    }
                                }
                                other => buf.push(other),
                            }
                        }
                        other => buf.push(other),
                    }
                }
                out.push(buf);
            }
            Some(_) => {
                // Unexpected token; abandon and return what we have.
                return out;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrips_simple() {
        let paths = vec![PathBuf::from("/a"), PathBuf::from("/b/c")];
        let s = encode_from_paths(&paths);
        assert_eq!(s, r#"["/a","/b/c"]"#);
        let back = decode_from_paths(&s);
        assert_eq!(back, vec!["/a".to_string(), "/b/c".to_string()]);
    }

    #[test]
    fn encode_escapes_quotes_and_backslashes() {
        let paths = vec![
            PathBuf::from(r#"/has "quote""#),
            PathBuf::from(r"/back\slash"),
        ];
        let s = encode_from_paths(&paths);
        let back = decode_from_paths(&s);
        assert_eq!(back[0], r#"/has "quote""#);
        assert_eq!(back[1], r"/back\slash");
    }

    #[test]
    fn decode_returns_empty_on_garbage() {
        assert!(decode_from_paths("not json").is_empty());
        assert!(decode_from_paths("").is_empty());
    }

    #[test]
    fn open_finish_run_roundtrip() {
        let mut state = State::open_in_memory().unwrap();
        let id = state
            .open_run("photos", &[PathBuf::from("/x")], false, false)
            .unwrap();
        let row = state.get_run(id).unwrap().unwrap();
        assert_eq!(row.profile, "photos");
        assert_eq!(row.kind, "run");
        assert_eq!(row.from_paths, vec!["/x".to_string()]);
        assert!(row.finished_at.is_none());

        let counts = RunCounts {
            placed: 3,
            ..RunCounts::default()
        };
        state.finish_run(id, &counts).unwrap();
        let row = state.get_run(id).unwrap().unwrap();
        assert!(row.finished_at.is_some());
        assert_eq!(row.counts.placed, 3);
    }

    #[test]
    fn list_runs_returns_newest_first() {
        let mut state = State::open_in_memory().unwrap();
        let a = state.open_run("photos", &[], false, false).unwrap();
        let b = state.open_run("photos", &[], false, false).unwrap();
        let rows = state.list_runs(None, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id.0, b.0);
        assert_eq!(rows[1].id.0, a.0);
    }

    #[test]
    fn list_runs_filters_by_profile() {
        let mut state = State::open_in_memory().unwrap();
        let _ = state.open_run("photos", &[], false, false).unwrap();
        let docs = state.open_run("docs", &[], false, false).unwrap();
        let rows = state.list_runs(Some("docs"), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id.0, docs.0);
    }

    #[test]
    fn list_runs_respects_limit() {
        let mut state = State::open_in_memory().unwrap();
        for _ in 0..5 {
            state.open_run("photos", &[], false, false).unwrap();
        }
        let rows = state.list_runs(None, 2).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn finish_revert_run_updates_target() {
        let mut state = State::open_in_memory().unwrap();
        let target = state.open_run("photos", &[], false, false).unwrap();
        state.finish_run(target, &RunCounts::default()).unwrap();
        let revert = state.open_revert_run("photos", target).unwrap();
        state
            .finish_revert_run(revert, target, &RunCounts::default())
            .unwrap();

        let target_row = state.get_run(target).unwrap().unwrap();
        assert!(target_row.reverted_at.is_some());
        assert_eq!(target_row.reverted_by.map(|r| r.0), Some(revert.0));

        let revert_row = state.get_run(revert).unwrap().unwrap();
        assert_eq!(revert_row.kind, "revert");
        assert_eq!(revert_row.target_run_id.map(|r| r.0), Some(target.0));
    }
}
