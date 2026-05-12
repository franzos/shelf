//! `shelf verify`: walks `placements`, rehashes each row's `dest_path`, and
//! reports drift / missing-destination relative to the joined `files.sha256`.
//!
//! Complement of `shelf health`, which checks the source side. Reservation
//! rows (`dest_path LIKE ':reserved:%'`) are excluded.

use std::path::PathBuf;

use rusqlite::params;

use crate::error::{Error, Result};
use crate::hash::{hex, sha256_file};
use crate::plan::{HealthEntry, HealthKind};
use crate::state::{HealthRow, State};

#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct VerifyReport {
    pub entries: Vec<HealthEntry>,
    pub total_placements: usize,
    pub checked: usize,
}

impl VerifyReport {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Selection mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Full,
    /// Rehash `N` randomly-selected placements. `N == 0` is a no-op.
    Sample(usize),
}

impl Mode {
    /// Default: ~1% of `total`, floor 1, zero on empty.
    #[must_use]
    pub fn default_sample_count(total: usize) -> usize {
        if total == 0 {
            return 0;
        }
        let pct = total.div_ceil(100);
        pct.max(1)
    }
}

/// Drive a verify pass. Writes drift / missing-destination rows to the
/// `health` table inside a single transaction. The connection must be
/// writable.
pub fn run(state: &mut State, mode: Mode) -> Result<VerifyReport> {
    let total = count_placements(state)?;
    let to_check = match mode {
        Mode::Full => total,
        Mode::Sample(n) => n.min(total),
    };

    if to_check == 0 {
        return Ok(VerifyReport {
            entries: Vec::new(),
            total_placements: total,
            checked: 0,
        });
    }

    let rows = fetch_targets(state, mode, to_check)?;
    let checked = rows.len();

    let mut entries: Vec<HealthEntry> = Vec::new();
    for row in &rows {
        if let Some(entry) = inspect(row)? {
            entries.push(entry);
        }
    }

    if !entries.is_empty() {
        persist(state, &entries)?;
    }

    Ok(VerifyReport {
        entries,
        total_placements: total,
        checked,
    })
}

struct Target {
    dest_path: String,
    expected_sha: String,
}

fn count_placements(state: &State) -> Result<usize> {
    let db_path = state.db_path().to_path_buf();
    let conn = state.conn();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM placements \
             WHERE dest_path NOT LIKE ':reserved:%'",
            [],
            |r| r.get(0),
        )
        .map_err(|source| Error::Sqlite {
            path: db_path,
            source,
        })?;
    Ok(usize::try_from(n).unwrap_or(0))
}

fn fetch_targets(state: &State, mode: Mode, limit: usize) -> Result<Vec<Target>> {
    let db_path = state.db_path().to_path_buf();
    let conn = state.conn();
    let sql = match mode {
        Mode::Full => {
            "SELECT p.dest_path, f.sha256 \
             FROM placements p JOIN files f ON p.file_id = f.id \
             WHERE p.dest_path NOT LIKE ':reserved:%' \
             ORDER BY p.id \
             LIMIT ?1"
        }
        Mode::Sample(_) => {
            "SELECT p.dest_path, f.sha256 \
             FROM placements p JOIN files f ON p.file_id = f.id \
             WHERE p.dest_path NOT LIKE ':reserved:%' \
             ORDER BY RANDOM() \
             LIMIT ?1"
        }
    };
    let mut stmt = conn.prepare(sql).map_err(|source| Error::Sqlite {
        path: db_path.clone(),
        source,
    })?;
    let limit_i = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = stmt
        .query_map(params![limit_i], |r| {
            Ok(Target {
                dest_path: r.get::<_, String>(0)?,
                expected_sha: r.get::<_, String>(1)?,
            })
        })
        .map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?);
    }
    Ok(out)
}

fn inspect(target: &Target) -> Result<Option<HealthEntry>> {
    let path = PathBuf::from(&target.dest_path);
    if !path.exists() {
        return Ok(Some(HealthEntry {
            kind: HealthKind::MissingDestination,
            path,
            detail: None,
        }));
    }
    let actual = sha256_file(&path)?;
    let actual_hex = hex(&actual);
    if actual_hex != target.expected_sha {
        return Ok(Some(HealthEntry {
            kind: HealthKind::Drift,
            path,
            detail: Some(format!(
                "expected={} got={}",
                short8(&target.expected_sha),
                short8(&actual_hex)
            )),
        }));
    }
    Ok(None)
}

fn persist(state: &mut State, entries: &[HealthEntry]) -> Result<()> {
    let path_strings: Vec<String> = entries
        .iter()
        .map(|e| e.path.to_string_lossy().into_owned())
        .collect();
    let rows: Vec<HealthRow<'_>> = entries
        .iter()
        .zip(path_strings.iter())
        .filter_map(|(e, p)| {
            let kind = match e.kind {
                HealthKind::Drift => "drift",
                HealthKind::MissingDestination => "missing-destination",
                _ => return None,
            };
            Some(HealthRow {
                kind,
                dest_path: p.as_str(),
                detail: e.detail.as_deref(),
            })
        })
        .collect();
    state.record_health_many(&rows)
}

fn short8(hex_str: &str) -> &str {
    if hex_str.len() >= 8 {
        &hex_str[..8]
    } else {
        hex_str
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sample_zero_for_empty_library() {
        assert_eq!(Mode::default_sample_count(0), 0);
    }

    #[test]
    fn default_sample_floor_is_one() {
        assert_eq!(Mode::default_sample_count(1), 1);
        assert_eq!(Mode::default_sample_count(50), 1);
        assert_eq!(Mode::default_sample_count(99), 1);
    }

    #[test]
    fn default_sample_one_percent_rounded_up() {
        assert_eq!(Mode::default_sample_count(100), 1);
        assert_eq!(Mode::default_sample_count(101), 2);
        assert_eq!(Mode::default_sample_count(200), 2);
        assert_eq!(Mode::default_sample_count(250), 3);
        assert_eq!(Mode::default_sample_count(10_000), 100);
    }
}
