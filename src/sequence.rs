//! Sequence numbering — assignment and persistence.
//!
//! Numbers are bucketed by `(output_name, scope_key)` where `scope_key` is
//! derived from the file's `taken_at` and the profile's [`SequenceScope`]:
//!
//! | scope    | scope_key                                          |
//! | -------- | -------------------------------------------------- |
//! | `Global` | `""`                                               |
//! | `Year`   | `"YYYY"`                                           |
//! | `Month`  | `"YYYY-MM"`                                        |
//! | `Day`    | `"YYYY-MM-DD"`                                     |
//! | `Folder` | rendered destination directory (M8 supplies this)  |
//!
//! `Folder` scope hinges on the **destination** directory — sibling files
//! landing in the same `{yyyy}/{mm}` share a counter regardless of source.
//!
//! Two entry points:
//!
//! - [`Sequencer::assign`] writes a placeholder placement row with a synthetic
//!   `dest_path` (`":reserved:<file_id>:<output_name>"`) carrying the assigned
//!   seq. M9 later `UPDATE`s the row to swap in the rendered destination.
//! - [`Sequencer::peek_next`] runs the same probe without writing, so dry-run
//!   leaves no reservation rows. The planner tracks in-batch advances.
//!
//! Order-independence is the caller's contract: callers must feed files in
//! `(taken_at ASC, source_path ASC)` order so assignment is stable across
//! reruns.

use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::config::{Profile, SequenceScope};
use crate::error::{Error, Result};
use crate::state::{FileId, State};

const SYSTEM_TS_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3fZ";

pub struct Sequencer<'a> {
    state: &'a mut State,
    profile: &'a Profile,
}

impl<'a> Sequencer<'a> {
    #[must_use]
    pub fn new(state: &'a mut State, profile: &'a Profile) -> Self {
        Self { state, profile }
    }

    /// Mutable access to the underlying [`State`] so the planner can issue
    /// dedupe/conflict queries through the same handle.
    pub fn state_mut(&mut self) -> &mut State {
        self.state
    }

    /// Hand out a stable seq for `(file_id, output_name)`.
    ///
    /// 1. If a placement row already exists with a non-NULL `seq`, return it.
    /// 2. Else probe `MAX(seq) + 1` within `(output_name, scope_key)`, or
    ///    `profile.sequence.start` if the bucket is empty.
    /// 3. Insert a placeholder row carrying the assigned seq.
    pub fn assign(&mut self, file_id: FileId, output_name: &str, scope_key: &str) -> Result<u64> {
        let db_path = self.state.db_path().to_path_buf();

        if let Some(seq) = probe_existing_seq(self.state.conn(), &db_path, file_id, output_name)? {
            return Ok(seq);
        }

        let next = probe_next_seq(
            self.state.conn(),
            &db_path,
            output_name,
            scope_key,
            self.profile.sequence.start,
        )?;

        let placeholder = reserved_dest_path(file_id, output_name);
        let now = now_iso();
        let next_i64 = i64::try_from(next).unwrap_or(i64::MAX);

        self.state
            .conn()
            .execute(
                "INSERT INTO placements \
                    (file_id, output_name, dest_path, seq, placed_at, scope_key) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    file_id.0,
                    output_name,
                    &placeholder,
                    next_i64,
                    &now,
                    scope_key,
                ],
            )
            .map_err(|e| Error::Sqlite {
                path: db_path,
                source: e,
            })?;

        Ok(next)
    }

    /// Read-only sibling of [`Sequencer::assign`]: returns the seq that would
    /// be assigned without writing. Two consecutive `peek_next` calls within
    /// the same bucket return the **same** number — the planner is responsible
    /// for advancing the in-batch counter.
    pub fn peek_next(
        &mut self,
        file_id: FileId,
        output_name: &str,
        scope_key: &str,
    ) -> Result<u64> {
        let db_path = self.state.db_path().to_path_buf();

        if let Some(seq) = probe_existing_seq(self.state.conn(), &db_path, file_id, output_name)? {
            return Ok(seq);
        }

        probe_next_seq(
            self.state.conn(),
            &db_path,
            output_name,
            scope_key,
            self.profile.sequence.start,
        )
    }

    /// Derive the scope_key for a file. `dest_directory` is only consulted
    /// when `scope == Folder`; pass `None` otherwise. A missing directory
    /// under `Folder` falls back to `""`.
    #[must_use]
    pub fn scope_key(
        scope: SequenceScope,
        taken_at: &NaiveDateTime,
        dest_directory: Option<&str>,
    ) -> String {
        use chrono::Datelike;
        match scope {
            SequenceScope::Global => String::new(),
            SequenceScope::Year => format!("{:04}", taken_at.year()),
            SequenceScope::Month => {
                format!("{:04}-{:02}", taken_at.year(), taken_at.month())
            }
            SequenceScope::Day => format!(
                "{:04}-{:02}-{:02}",
                taken_at.year(),
                taken_at.month(),
                taken_at.day()
            ),
            SequenceScope::Folder => dest_directory.unwrap_or("").to_string(),
        }
    }
}

fn probe_existing_seq(
    conn: &Connection,
    db_path: &std::path::Path,
    file_id: FileId,
    output_name: &str,
) -> Result<Option<u64>> {
    let existing: Option<i64> = conn
        .query_row(
            "SELECT seq FROM placements \
             WHERE file_id = ?1 AND output_name = ?2 AND seq IS NOT NULL \
             LIMIT 1",
            params![file_id.0, output_name],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|e| Error::Sqlite {
            path: db_path.to_path_buf(),
            source: e,
        })?;
    Ok(existing.map(u64_from_i64))
}

fn probe_next_seq(
    conn: &Connection,
    db_path: &std::path::Path,
    output_name: &str,
    scope_key: &str,
    start: u64,
) -> Result<u64> {
    let max_seq: Option<i64> = conn
        .query_row(
            "SELECT MAX(seq) FROM placements \
             WHERE output_name = ?1 AND scope_key = ?2",
            params![output_name, scope_key],
            |row| row.get::<_, Option<i64>>(0),
        )
        .map_err(|e| Error::Sqlite {
            path: db_path.to_path_buf(),
            source: e,
        })?;

    Ok(match max_seq {
        Some(m) if m >= 0 => u64_from_i64(m) + 1,
        _ => start,
    })
}

/// Synthetic `dest_path` for a reserved placement row; unique under
/// `UNIQUE(output_name, dest_path)`.
fn reserved_dest_path(file_id: FileId, output_name: &str) -> String {
    format!(":reserved:{}:{output_name}", file_id.0)
}

fn now_iso() -> String {
    let now: DateTime<Utc> = Utc::now();
    now.format(SYSTEM_TS_FORMAT).to_string()
}

fn u64_from_i64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

#[cfg(test)]
mod scope_key_tests {
    use super::*;
    use chrono::NaiveDate;

    fn dt(y: i32, m: u32, d: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

    #[test]
    fn global_scope_is_empty() {
        let k = Sequencer::scope_key(SequenceScope::Global, &dt(2024, 3, 15), None);
        assert_eq!(k, "");
    }

    #[test]
    fn year_scope_is_yyyy() {
        let k = Sequencer::scope_key(SequenceScope::Year, &dt(2024, 3, 15), None);
        assert_eq!(k, "2024");
    }

    #[test]
    fn month_scope_is_yyyy_mm() {
        let k = Sequencer::scope_key(SequenceScope::Month, &dt(2024, 3, 15), None);
        assert_eq!(k, "2024-03");
    }

    #[test]
    fn day_scope_is_yyyy_mm_dd() {
        let k = Sequencer::scope_key(SequenceScope::Day, &dt(2024, 3, 15), None);
        assert_eq!(k, "2024-03-15");
    }

    #[test]
    fn folder_scope_uses_destination_directory() {
        let k = Sequencer::scope_key(SequenceScope::Folder, &dt(2024, 3, 15), Some("2024/03"));
        assert_eq!(k, "2024/03");
    }

    #[test]
    fn folder_scope_falls_back_when_directory_missing() {
        let k = Sequencer::scope_key(SequenceScope::Folder, &dt(2024, 3, 15), None);
        assert_eq!(k, "");
    }
}
