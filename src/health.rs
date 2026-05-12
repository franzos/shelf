//! Health checks for `shelf health`.
//!
//! Read-only diagnostics over a configured profile. Combines persisted
//! entries from the `health` table with fresh runtime checks (truncation,
//! orphans, recomputed missing-date / unclassified, and a drift spot-check
//! over the N most recent `files` rows).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use rusqlite::params;
use walkdir::WalkDir;

use crate::config::Profile;
use crate::error::{Error, Result};
use crate::hash::{hex, sha256_file};
use crate::metadata::quicktime::container_ok;
use crate::plan::{HealthEntry, HealthKind};
use crate::state::State;

/// Default number of recent `files` rows to spot-check for hash drift.
pub const DEFAULT_SAMPLE: usize = 100;

/// Aggregated output of a `shelf health` invocation.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct HealthReport {
    pub entries: Vec<HealthEntry>,
}

impl HealthReport {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Run all health checks against `profile`. `state` must be opened
/// read-only — this routine never writes.
pub fn check(profile: &Profile, state: &State, sample: usize) -> Result<HealthReport> {
    let mut entries: Vec<HealthEntry> = Vec::new();

    entries.extend(read_persisted_health(state)?);

    entries.extend(recompute_missing_date(state)?);
    entries.extend(recompute_unclassified(profile, state)?);

    for output in &profile.outputs {
        if !output.path.exists() {
            tracing::debug!(
                output = %output.name,
                path = %output.path.display(),
                "health: output path missing on disk, skipping orphan/truncation walks"
            );
            continue;
        }
        let placed = load_placements_for_output(state, &output.name)?;
        for path in walk_output_files(&output.path) {
            let path_str = path.to_string_lossy().into_owned();
            if placed.binary_search(&path_str).is_err() {
                entries.push(HealthEntry {
                    kind: HealthKind::Orphan,
                    path: path.clone(),
                    detail: Some(format!("output={}", output.name)),
                });
            }
            if let Some(entry) = truncation_entry(&path)? {
                entries.push(entry);
            }
        }
    }

    entries.extend(drift_spot_check(state, sample)?);

    Ok(HealthReport { entries })
}

/// JPEG truncation check.
///
/// A complete JPEG starts with `FF D8` (SOI) and ends with `FF D9` (EOI).
/// Files that don't start with `FF D8` aren't JPEGs and aren't this
/// function's problem (return `Ok(false)`); files that start with `FF D8`
/// but don't end with `FF D9` — or are shorter than two bytes — are
/// truncated.
pub fn jpeg_is_truncated(path: &Path) -> Result<bool> {
    let mut file = File::open(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let size = file
        .metadata()
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if size < 2 {
        return Ok(true);
    }
    let mut head = [0u8; 2];
    file.read_exact(&mut head).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if head != [0xFF, 0xD8] {
        return Ok(false);
    }
    let mut tail = [0u8; 2];
    file.seek(SeekFrom::End(-2)).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.read_exact(&mut tail).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(tail != [0xFF, 0xD9])
}

/// MP4/MOV truncation check. Files that aren't MP4/MOV at all (e.g. a JPEG
/// passed in by mistake) return `Ok(true)` — callers should dispatch by
/// extension and only invoke this on `.mp4` / `.mov` / `.m4v`.
pub fn mp4_is_truncated(path: &Path) -> Result<bool> {
    Ok(!container_ok(path)?)
}

fn truncation_entry(path: &Path) -> Result<Option<HealthEntry>> {
    let Some(ext) = path
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return Ok(None);
    };
    let truncated = match ext.as_str() {
        "jpg" | "jpeg" => jpeg_is_truncated(path)?,
        "mp4" | "mov" | "m4v" => mp4_is_truncated(path)?,
        _ => return Ok(None),
    };
    if truncated {
        Ok(Some(HealthEntry {
            kind: HealthKind::Truncated,
            path: path.to_path_buf(),
            detail: None,
        }))
    } else {
        Ok(None)
    }
}

/// Walk `root` recursively, yielding regular files. Skips the in-flight
/// `.shelf-tmp-*` sidecars apply may leave behind on a crashed apply.
fn walk_output_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let is_tmp = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains(".shelf-tmp-"));
        if is_tmp {
            continue;
        }
        out.push(path.to_path_buf());
    }
    out
}

/// Pull every `dest_path` recorded for `output_name`, excluding the
/// `:reserved:` sentinels the sequencer writes during apply transactions.
/// The returned vec is sorted so callers can `binary_search` on it.
fn load_placements_for_output(state: &State, output_name: &str) -> Result<Vec<String>> {
    let db_path = state.db_path().to_path_buf();
    let conn = state.conn();
    let mut stmt = conn
        .prepare(
            "SELECT dest_path FROM placements \
             WHERE output_name = ?1 AND dest_path NOT LIKE ':reserved:%'",
        )
        .map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;
    let rows = stmt
        .query_map(params![output_name], |r| r.get::<_, String>(0))
        .map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;
    let mut out: Vec<String> = Vec::new();
    for row in rows {
        let p = row.map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;
        out.push(p);
    }
    out.sort();
    Ok(out)
}

fn recompute_missing_date(state: &State) -> Result<Vec<HealthEntry>> {
    let db_path = state.db_path().to_path_buf();
    let conn = state.conn();
    let mut stmt = conn
        .prepare("SELECT source_path FROM files WHERE taken_at_source = 'mtime'")
        .map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;
    let mut out = Vec::new();
    for row in rows {
        let path = row.map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;
        out.push(HealthEntry {
            kind: HealthKind::MissingDate,
            path: PathBuf::from(path),
            detail: Some("taken_at_source=mtime".to_string()),
        });
    }
    Ok(out)
}

fn recompute_unclassified(_profile: &Profile, state: &State) -> Result<Vec<HealthEntry>> {
    let db_path = state.db_path().to_path_buf();
    let conn = state.conn();
    let mut stmt = conn
        .prepare("SELECT source_path FROM files WHERE kind = ?1")
        .map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;
    let rows = stmt
        .query_map(params![crate::kind::OTHER], |r| r.get::<_, String>(0))
        .map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;
    let mut out = Vec::new();
    for row in rows {
        let path = row.map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;
        out.push(HealthEntry {
            kind: HealthKind::Unclassified,
            path: PathBuf::from(path),
            detail: None,
        });
    }
    Ok(out)
}

fn read_persisted_health(state: &State) -> Result<Vec<HealthEntry>> {
    let db_path = state.db_path().to_path_buf();
    let conn = state.conn();
    let mut stmt = conn
        .prepare(
            "SELECT h.kind, h.detail, f.source_path \
             FROM health h LEFT JOIN files f ON h.file_id = f.id",
        )
        .map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;
    let mut out = Vec::new();
    for row in rows {
        let (kind_str, detail, source_path) = row.map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;
        let Some(kind) = parse_health_kind(&kind_str) else {
            continue;
        };
        out.push(HealthEntry {
            kind,
            path: PathBuf::from(source_path.unwrap_or_default()),
            detail,
        });
    }
    Ok(out)
}

/// Inverse of [`crate::run::health_kind_str`]. Unknown strings are silently
/// dropped so a future migration adding a new kind doesn't blow up older
/// binaries.
fn parse_health_kind(s: &str) -> Option<HealthKind> {
    match s {
        "walk-error" => Some(HealthKind::WalkError),
        "missing-date" => Some(HealthKind::MissingDate),
        "unclassified" => Some(HealthKind::Unclassified),
        "unrouted" => Some(HealthKind::Unrouted),
        "extract-failed" => Some(HealthKind::ExtractFailed),
        "hash-failed" => Some(HealthKind::HashFailed),
        "truncated" => Some(HealthKind::Truncated),
        "drift" => Some(HealthKind::Drift),
        "orphan" => Some(HealthKind::Orphan),
        "missing-source" => Some(HealthKind::MissingSource),
        "missing-destination" => Some(HealthKind::MissingDestination),
        _ => None,
    }
}

fn drift_spot_check(state: &State, sample: usize) -> Result<Vec<HealthEntry>> {
    if sample == 0 {
        return Ok(Vec::new());
    }
    let db_path = state.db_path().to_path_buf();
    let conn = state.conn();
    let mut stmt = conn
        .prepare(
            "SELECT source_path, sha256 FROM files \
             ORDER BY last_seen DESC LIMIT ?1",
        )
        .map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;
    let limit = i64::try_from(sample).unwrap_or(i64::MAX);
    let rows = stmt
        .query_map(params![limit], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;

    let mut entries = Vec::new();
    for row in rows {
        let (source_path_str, recorded_hex) = row.map_err(|source| Error::Sqlite {
            path: db_path.clone(),
            source,
        })?;
        let path = PathBuf::from(&source_path_str);
        if !path.exists() {
            entries.push(HealthEntry {
                kind: HealthKind::MissingSource,
                path,
                detail: None,
            });
            continue;
        }
        let actual = sha256_file(&path)?;
        let actual_hex = hex(&actual);
        if actual_hex != recorded_hex {
            entries.push(HealthEntry {
                kind: HealthKind::Drift,
                path,
                detail: Some(format!(
                    "expected={} got={}",
                    short8(&recorded_hex),
                    short8(&actual_hex)
                )),
            });
        }
    }
    Ok(entries)
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
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn jpeg_truncation_detects_missing_eoi() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.jpg");
        std::fs::write(&path, [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]).unwrap();
        assert!(jpeg_is_truncated(&path).unwrap());
    }

    #[test]
    fn jpeg_truncation_passes_well_formed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ok.jpg");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&[0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xD9]).unwrap();
        assert!(!jpeg_is_truncated(&path).unwrap());
    }

    #[test]
    fn jpeg_truncation_ignores_non_jpeg() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope.dat");
        std::fs::write(&path, b"hello").unwrap();
        assert!(!jpeg_is_truncated(&path).unwrap());
    }

    #[test]
    fn jpeg_truncation_flags_tiny_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tiny.jpg");
        std::fs::write(&path, [0xFF]).unwrap();
        assert!(jpeg_is_truncated(&path).unwrap());
    }

    #[test]
    fn truncation_entry_dispatches_by_extension() {
        let dir = TempDir::new().unwrap();
        let bad = dir.path().join("bad.jpg");
        std::fs::write(&bad, [0xFF, 0xD8, 0x00]).unwrap();
        let entry = truncation_entry(&bad).unwrap().expect("expected entry");
        assert_eq!(entry.kind, HealthKind::Truncated);

        let pdf = dir.path().join("doc.pdf");
        std::fs::write(&pdf, b"%PDF-1.4").unwrap();
        assert!(truncation_entry(&pdf).unwrap().is_none());
    }
}
