//! Applier — execute a [`Plan`] against the filesystem and state DB.
//!
//! Writes for copy/move stream to a temp sibling (`<dst>.shelf-tmp-<rand>`),
//! fsync, then atomic rename. A crash before rename leaves the destination
//! untouched. `fs::rename` overwrites atomically on Unix only; Windows is
//! deliberately unimplemented.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use filetime::FileTime;
use rusqlite::params;

use crate::config::{OpMode, Profile};
use crate::error::{ApplyErrorKind, Error, Result};
use crate::plan::{Plan, PlannedAction};
use crate::state::{FileId, RunId, State};

const SYSTEM_TS_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3fZ";

/// Summary returned by [`apply`].
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct ApplyReport {
    pub placed: u64,
    pub replaced: u64,
    pub skipped_duplicate: u64,
    pub skipped_conflict: u64,
    pub failed: Vec<ApplyFailure>,
}

/// One per-action failure carried in [`ApplyReport::failed`].
#[derive(Debug)]
#[non_exhaustive]
pub struct ApplyFailure {
    pub action: PlannedAction,
    pub error: Error,
}

/// Execute a [`Plan`] against the filesystem and state DB.
///
/// Per-action io errors are collected into [`ApplyReport::failed`]; only
/// catastrophic SQL errors short-circuit with `Err`.
pub fn apply(
    state: &mut State,
    profile: &Profile,
    plan: &Plan,
    run_id: Option<RunId>,
) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();
    let preserve_by_output = preserve_mtime_index(profile);
    for action in &plan.actions {
        apply_one(state, action, &preserve_by_output, run_id, &mut report)?;
    }
    Ok(report)
}

fn preserve_mtime_index(profile: &Profile) -> HashMap<&str, bool> {
    profile
        .outputs
        .iter()
        .map(|o| (o.name.as_str(), o.preserve_mtime))
        .collect()
}

fn apply_one(
    state: &mut State,
    action: &PlannedAction,
    preserve_by_output: &HashMap<&str, bool>,
    run_id: Option<RunId>,
    report: &mut ApplyReport,
) -> Result<()> {
    match action {
        PlannedAction::Place {
            src,
            dst,
            mode,
            output_name,
            file_id,
            seq,
            scope_key,
            ..
        } => {
            let preserve = preserve_by_output
                .get(output_name.as_str())
                .copied()
                .unwrap_or(true);
            match place_file(src, dst, *mode, preserve) {
                Ok(()) => {
                    record_placement(
                        state,
                        *file_id,
                        output_name,
                        dst,
                        *seq,
                        scope_key,
                        run_id,
                        *mode,
                    )?;
                    report.placed += 1;
                    tracing::info!(
                        output = %output_name,
                        dst = %dst.display(),
                        "placed"
                    );
                }
                Err(e) => report.failed.push(ApplyFailure {
                    action: action.clone(),
                    error: e,
                }),
            }
        }
        PlannedAction::Replace {
            src,
            dst,
            mode,
            output_name,
            file_id,
            seq,
            scope_key,
            ..
        } => {
            let preserve = preserve_by_output
                .get(output_name.as_str())
                .copied()
                .unwrap_or(true);
            match place_file(src, dst, *mode, preserve) {
                Ok(()) => {
                    replace_placement(
                        state,
                        *file_id,
                        output_name,
                        dst,
                        *seq,
                        scope_key,
                        run_id,
                        *mode,
                    )?;
                    report.replaced += 1;
                    tracing::info!(
                        output = %output_name,
                        dst = %dst.display(),
                        "replaced"
                    );
                }
                Err(e) => report.failed.push(ApplyFailure {
                    action: action.clone(),
                    error: e,
                }),
            }
        }
        PlannedAction::SkipDuplicate {
            src,
            existing_dst,
            output_name,
        } => {
            report.skipped_duplicate += 1;
            tracing::info!(
                output = %output_name,
                src = %src.display(),
                existing = %existing_dst.display(),
                "skip: duplicate"
            );
        }
        PlannedAction::SkipConflict {
            src,
            dst,
            output_name,
        } => {
            report.skipped_conflict += 1;
            tracing::info!(
                output = %output_name,
                src = %src.display(),
                dst = %dst.display(),
                "skip: conflict"
            );
        }
    }
    Ok(())
}

fn place_file(src: &Path, dst: &Path, mode: OpMode, preserve_mtime: bool) -> Result<()> {
    if let Some(parent) = dst.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| Error::Apply {
            kind: ApplyErrorKind::CreateDir,
            path: parent.to_path_buf(),
            source,
        })?;
    }

    match mode {
        OpMode::Copy => {
            copy_via_tempfile(src, dst)?;
            if preserve_mtime {
                copy_mtime(src, dst)?;
            }
            Ok(())
        }
        OpMode::Move => {
            // Same-fs rename preserves mtime; EXDEV fallback handles the
            // stamp internally because it must capture src's mtime before
            // unlinking it.
            move_or_fallback(src, dst, preserve_mtime).map(|_| ())
        }
        OpMode::Hardlink => fs::hard_link(src, dst).map_err(|source| Error::Apply {
            kind: ApplyErrorKind::Hardlink,
            path: dst.to_path_buf(),
            source,
        }),
        OpMode::Symlink => symlink_absolute(src, dst),
    }
}

fn copy_mtime(src: &Path, dst: &Path) -> Result<()> {
    let src_meta = fs::metadata(src).map_err(|source| Error::Apply {
        kind: ApplyErrorKind::Copy,
        path: src.to_path_buf(),
        source,
    })?;
    let mtime = FileTime::from_last_modification_time(&src_meta);
    filetime::set_file_mtime(dst, mtime).map_err(|source| Error::Apply {
        kind: ApplyErrorKind::Copy,
        path: dst.to_path_buf(),
        source,
    })
}

fn copy_via_tempfile(src: &Path, dst: &Path) -> Result<()> {
    let tmp = temp_sibling(dst);
    match do_copy_fsync_rename(src, &tmp, dst) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn do_copy_fsync_rename(src: &Path, tmp: &Path, dst: &Path) -> Result<()> {
    fs::copy(src, tmp).map_err(|source| Error::Apply {
        kind: ApplyErrorKind::Copy,
        path: tmp.to_path_buf(),
        source,
    })?;
    let f = fs::OpenOptions::new()
        .read(true)
        .open(tmp)
        .map_err(|source| Error::Apply {
            kind: ApplyErrorKind::Fsync,
            path: tmp.to_path_buf(),
            source,
        })?;
    f.sync_all().map_err(|source| Error::Apply {
        kind: ApplyErrorKind::Fsync,
        path: tmp.to_path_buf(),
        source,
    })?;
    drop(f);
    fs::rename(tmp, dst).map_err(|source| Error::Apply {
        kind: ApplyErrorKind::Rename,
        path: dst.to_path_buf(),
        source,
    })
}

/// `fs::rename` for same-fs moves; falls back to copy+remove on EXDEV.
/// Returns `true` when the EXDEV fallback ran.
fn move_or_fallback(src: &Path, dst: &Path, preserve_mtime: bool) -> Result<bool> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(false),
        Err(e) if is_cross_device(&e) => {
            // Capture src's mtime before the unlink so the caller can stamp
            // dst even though `fs::remove_file(src)` below makes src
            // unreadable.
            let captured = if preserve_mtime {
                Some(
                    fs::metadata(src)
                        .map(|m| FileTime::from_last_modification_time(&m))
                        .map_err(|source| Error::Apply {
                            kind: ApplyErrorKind::Copy,
                            path: src.to_path_buf(),
                            source,
                        })?,
                )
            } else {
                None
            };
            copy_via_tempfile(src, dst)?;
            if let Some(mt) = captured {
                filetime::set_file_mtime(dst, mt).map_err(|source| Error::Apply {
                    kind: ApplyErrorKind::Copy,
                    path: dst.to_path_buf(),
                    source,
                })?;
            }
            fs::remove_file(src).map_err(|source| Error::Apply {
                kind: ApplyErrorKind::Remove,
                path: src.to_path_buf(),
                source,
            })?;
            Ok(true)
        }
        Err(source) => Err(Error::Apply {
            kind: ApplyErrorKind::Rename,
            path: dst.to_path_buf(),
            source,
        }),
    }
}

fn is_cross_device(e: &io::Error) -> bool {
    // ErrorKind::CrossesDevices is unstable; match the raw errno directly.
    matches!(e.raw_os_error(), Some(18))
}

#[cfg(unix)]
fn symlink_absolute(src: &Path, dst: &Path) -> Result<()> {
    let abs = absolutize(src);
    std::os::unix::fs::symlink(&abs, dst).map_err(|source| Error::Apply {
        kind: ApplyErrorKind::Symlink,
        path: dst.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn symlink_absolute(_src: &Path, dst: &Path) -> Result<()> {
    Err(Error::Apply {
        kind: ApplyErrorKind::Symlink,
        path: dst.to_path_buf(),
        source: io::Error::new(io::ErrorKind::Unsupported, "symlinks require unix"),
    })
}

fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(p))
        .unwrap_or_else(|_| p.to_path_buf())
}

/// `<dst>.shelf-tmp-<pid>-<nanos>-<counter>` — enough entropy without `rand`.
fn temp_sibling(dst: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let suffix = format!(".shelf-tmp-{pid}-{nanos:x}-{counter:x}");

    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    let name = dst
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    let mut new_name = name;
    new_name.push(suffix);
    parent.join(new_name)
}

/// Stable wire form for `placements.op_mode`.
pub fn op_mode_str(mode: OpMode) -> &'static str {
    match mode {
        OpMode::Copy => "copy",
        OpMode::Move => "move",
        OpMode::Hardlink => "hardlink",
        OpMode::Symlink => "symlink",
    }
}

#[allow(clippy::too_many_arguments)]
fn record_placement(
    state: &mut State,
    file_id: FileId,
    output_name: &str,
    dest_path: &Path,
    seq: u64,
    scope_key: &str,
    run_id: Option<RunId>,
    op_mode: OpMode,
) -> Result<()> {
    let db_path = state.db_path().to_path_buf();
    let dest_str = dest_path.to_string_lossy().into_owned();
    let now = now_iso();
    let seq_i64 = i64::try_from(seq).unwrap_or(i64::MAX);
    let run_id_param = run_id.map(|r| r.0);
    let op_mode_param = op_mode_str(op_mode);
    state
        .conn()
        .execute(
            "INSERT INTO placements \
                (file_id, output_name, dest_path, seq, placed_at, scope_key, \
                 run_id, op_mode) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                file_id.0,
                output_name,
                &dest_str,
                seq_i64,
                &now,
                scope_key,
                run_id_param,
                op_mode_param,
            ],
        )
        .map(|_| ())
        .map_err(|e| Error::Sqlite {
            path: db_path,
            source: e,
        })
}

/// Delete any prior placement row for `(output_name, dest_path)`, then insert.
/// Wrapped in a transaction so an observer never sees both rows.
#[allow(clippy::too_many_arguments)]
fn replace_placement(
    state: &mut State,
    file_id: FileId,
    output_name: &str,
    dest_path: &Path,
    seq: u64,
    scope_key: &str,
    run_id: Option<RunId>,
    op_mode: OpMode,
) -> Result<()> {
    let db_path = state.db_path().to_path_buf();
    let dest_str = dest_path.to_string_lossy().into_owned();
    let now = now_iso();
    let seq_i64 = i64::try_from(seq).unwrap_or(i64::MAX);
    let run_id_param = run_id.map(|r| r.0);
    let op_mode_param = op_mode_str(op_mode);
    let map_sql = |e: rusqlite::Error| Error::Sqlite {
        path: db_path.clone(),
        source: e,
    };

    let conn = state.conn();
    let tx = conn.unchecked_transaction().map_err(map_sql)?;
    tx.execute(
        "DELETE FROM placements WHERE output_name = ?1 AND dest_path = ?2",
        params![output_name, &dest_str],
    )
    .map_err(map_sql)?;
    tx.execute(
        "INSERT INTO placements \
            (file_id, output_name, dest_path, seq, placed_at, scope_key, \
             run_id, op_mode) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            file_id.0,
            output_name,
            &dest_str,
            seq_i64,
            &now,
            scope_key,
            run_id_param,
            op_mode_param,
        ],
    )
    .map_err(map_sql)?;
    tx.commit().map_err(map_sql)
}

fn now_iso() -> String {
    let now: DateTime<Utc> = Utc::now();
    now.format(SYSTEM_TS_FORMAT).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_sibling_lives_next_to_dst() {
        let dst = Path::new("/out/2024/03/photo.jpg");
        let t = temp_sibling(dst);
        assert_eq!(t.parent(), dst.parent());
        let name = t.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("photo.jpg.shelf-tmp-"));
    }

    #[test]
    fn temp_sibling_unique_across_calls() {
        let dst = Path::new("/out/photo.jpg");
        let a = temp_sibling(dst);
        let b = temp_sibling(dst);
        assert_ne!(a, b);
    }

    #[test]
    fn cross_device_detection_recognises_exdev() {
        let e = io::Error::from_raw_os_error(18);
        assert!(is_cross_device(&e));
        let e2 = io::Error::from_raw_os_error(2);
        assert!(!is_cross_device(&e2));
    }

    #[test]
    fn op_mode_str_is_stable_kebab_case() {
        assert_eq!(op_mode_str(OpMode::Copy), "copy");
        assert_eq!(op_mode_str(OpMode::Move), "move");
        assert_eq!(op_mode_str(OpMode::Hardlink), "hardlink");
        assert_eq!(op_mode_str(OpMode::Symlink), "symlink");
    }

    /// `replace_placement` deletes the prior row and inserts the new one
    /// inside a single transaction. If the INSERT fails (FK violation,
    /// constraint, etc.), the DELETE must roll back so the prior audit
    /// row survives. Without rollback, a failed Replace would leave a
    /// gap in `placements` and the disk artefact orphaned from any row.
    #[test]
    fn replace_placement_rolls_back_when_insert_fails() {
        let state_owned = State::open_in_memory().unwrap();
        let mut state = state_owned;
        state
            .conn()
            .execute(
                "INSERT INTO files ( \
                    source_path, size, mtime_secs, mtime_nanos, sha256, \
                    taken_at, taken_at_source, kind, first_seen, last_seen \
                 ) VALUES ('/src/a', 0, 0, 0, '00', '2024-01-01T00:00:00', 'mtime', 'photo', \
                           '2024-01-01T00:00:00.000Z', '2024-01-01T00:00:00.000Z')",
                [],
            )
            .unwrap();
        let file_id_a = FileId(1);

        record_placement(
            &mut state,
            file_id_a,
            "lib",
            Path::new("/dst1"),
            1,
            "2024-03",
            None,
            OpMode::Copy,
        )
        .unwrap();

        let before: i64 = state
            .conn()
            .query_row(
                "SELECT file_id FROM placements WHERE dest_path = '/dst1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, 1);

        let bogus = FileId(9999);
        let err = replace_placement(
            &mut state,
            bogus,
            "lib",
            Path::new("/dst1"),
            2,
            "2024-03",
            None,
            OpMode::Copy,
        );
        assert!(err.is_err(), "FK violation must surface as an error");

        let after_file_id: i64 = state
            .conn()
            .query_row(
                "SELECT file_id FROM placements WHERE dest_path = '/dst1'",
                [],
                |r| r.get(0),
            )
            .expect("prior row must survive a failed Replace");
        assert_eq!(
            after_file_id, 1,
            "the prior placement row must be intact after replace_placement rolls back"
        );

        let row_count: i64 = state
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM placements WHERE dest_path = '/dst1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(row_count, 1, "no extra rows from the failed INSERT");
    }
}
