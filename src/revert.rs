//! Revert a prior `shelf run`.
//!
//! Per op mode:
//! - `copy`, `hardlink`, `symlink`: delete `dest_path`, drop the placements row.
//! - `move`: move `dest_path` back to `files.source_path`, drop the row.
//!
//! Refused without `--force`:
//! - Destination sha drifted.
//! - Move-revert: source path already exists.
//!
//! Logged-and-cleaned (no `--force` needed):
//! - Destination missing: drop the row; file is already gone.
//! - Move-revert source parent missing: `create_dir_all`.
//!
//! Per-action failures land in [`RevertReport::failed`]; only SQL errors
//! short-circuit.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{ApplyErrorKind, Error, Result};
use crate::hash::{hex, sha256_file};
use crate::state::{PlacementRow, RunCounts, RunId, RunRow, State};

#[derive(Debug, Default)]
#[non_exhaustive]
pub struct RevertReport {
    pub reverted: u64,
    pub warnings: u64,
    pub failed: Vec<RevertFailure>,
}

#[derive(Debug)]
#[non_exhaustive]
pub struct RevertFailure {
    pub run_id: i64,
    pub op_mode: Option<String>,
    pub dest_path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone)]
enum RevertStep {
    Delete {
        dest: PathBuf,
        op_mode: String,
    },
    Restore {
        dest: PathBuf,
        src: PathBuf,
        op_mode: String,
    },
    /// Destination is missing on disk; drop the row only.
    DropRowOnly {
        dest: PathBuf,
        op_mode: String,
    },
}

#[derive(Debug)]
pub struct RevertOutcome {
    pub report: RevertReport,
    pub revert_run_id: Option<RunId>,
}

/// Refusal returned by [`precheck`] before any side effects.
#[derive(Debug)]
pub enum RevertRefusal {
    NotFound,
    DryRunTarget,
    RevertKindTarget,
    AlreadyReverted { reverted_by: Option<i64> },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RevertOptions {
    pub force: bool,
    pub dry_run: bool,
}

/// Look up the target run and decide whether revert is allowed.
pub fn precheck(
    state: &State,
    target_run_id: RunId,
    force: bool,
) -> Result<std::result::Result<RunRow, RevertRefusal>> {
    let Some(row) = state.get_run(target_run_id)? else {
        return Ok(Err(RevertRefusal::NotFound));
    };
    if row.dry_run {
        return Ok(Err(RevertRefusal::DryRunTarget));
    }
    if row.kind == "revert" {
        return Ok(Err(RevertRefusal::RevertKindTarget));
    }
    if row.reverted_at.is_some() && !force {
        return Ok(Err(RevertRefusal::AlreadyReverted {
            reverted_by: row.reverted_by.map(|r| r.0),
        }));
    }
    Ok(Ok(row))
}

/// Execute the revert.
pub fn revert<W: Write>(
    state: &mut State,
    profile_name: &str,
    target_run_id: RunId,
    options: RevertOptions,
    out: &mut W,
) -> Result<RevertOutcome> {
    let placements = state.placements_for_run(target_run_id)?;

    let mut steps = Vec::with_capacity(placements.len());
    let mut report = RevertReport::default();

    for p in &placements {
        match build_step(p, options.force) {
            Ok(step) => steps.push(step),
            Err(reason) => {
                report.failed.push(RevertFailure {
                    run_id: target_run_id.0,
                    op_mode: p.op_mode.clone(),
                    dest_path: p.dest_path.clone(),
                    reason: reason.clone(),
                });
                writeln!(
                    out,
                    "error\t{}\t{}\t{}\t({})",
                    target_run_id.0,
                    p.op_mode.as_deref().unwrap_or("?"),
                    p.dest_path.display(),
                    reason,
                )
                .map_err(io_stdout)?;
            }
        }
    }

    if options.dry_run {
        for step in &steps {
            print_step(step, target_run_id.0, &mut *out)?;
        }
        return Ok(RevertOutcome {
            report,
            revert_run_id: None,
        });
    }

    let revert_id = state.open_revert_run(profile_name, target_run_id)?;

    for step in &steps {
        match execute_step(state, revert_id, target_run_id, step, out) {
            Ok(executed) => match executed {
                ExecOutcome::Reverted => report.reverted += 1,
                ExecOutcome::Warned => report.warnings += 1,
            },
            Err(reason) => {
                report.failed.push(RevertFailure {
                    run_id: target_run_id.0,
                    op_mode: Some(step_op_mode(step).to_string()),
                    dest_path: step_dest(step).to_path_buf(),
                    reason: reason.clone(),
                });
                writeln!(
                    out,
                    "error\t{}\t{}\t{}\t({})",
                    target_run_id.0,
                    step_op_mode(step),
                    step_dest(step).display(),
                    reason,
                )
                .map_err(io_stdout)?;
            }
        }
    }

    let counts = RunCounts {
        placed: report.reverted,
        replaced: 0,
        skipped_duplicate: 0,
        skipped_conflict: 0,
        failed: report.failed.len() as u64,
        health: report.warnings,
    };
    state.finish_revert_run(revert_id, target_run_id, &counts)?;

    Ok(RevertOutcome {
        report,
        revert_run_id: Some(revert_id),
    })
}

enum ExecOutcome {
    Reverted,
    Warned,
}

fn build_step(p: &PlacementRow, force: bool) -> std::result::Result<RevertStep, String> {
    let op_mode = p.op_mode.clone().unwrap_or_else(|| "copy".to_string());
    let dest_exists = p.dest_path.exists();

    if !dest_exists {
        return Ok(RevertStep::DropRowOnly {
            dest: p.dest_path.clone(),
            op_mode,
        });
    }

    // Skip drift check for symlinks: hashing follows the link to source,
    // which is the same data anyway.
    if !force && op_mode != "symlink" {
        match sha256_file(&p.dest_path) {
            Ok(bytes) => {
                if hex(&bytes) != p.sha256 {
                    return Err("drift detected; use --force to override".to_string());
                }
            }
            Err(e) => {
                return Err(format!("could not rehash dest for drift check: {e}"));
            }
        }
    }

    match op_mode.as_str() {
        "move" => {
            if p.source_path.exists() && !force {
                return Err(
                    "move-revert: source already exists; use --force to overwrite".to_string(),
                );
            }
            Ok(RevertStep::Restore {
                dest: p.dest_path.clone(),
                src: p.source_path.clone(),
                op_mode,
            })
        }
        _ => Ok(RevertStep::Delete {
            dest: p.dest_path.clone(),
            op_mode,
        }),
    }
}

fn execute_step<W: Write>(
    state: &mut State,
    revert_run_id: RunId,
    target_run_id: RunId,
    step: &RevertStep,
    out: &mut W,
) -> std::result::Result<ExecOutcome, String> {
    match step {
        RevertStep::Delete { dest, op_mode } => {
            fs::remove_file(dest).map_err(|e| format!("remove failed: {e}"))?;
            state
                .delete_placement_for_run(target_run_id, &dest.to_string_lossy())
                .map_err(|e| format!("db delete: {e}"))?;
            // delete is keyed on (target_run_id, dest); revert_run_id unused here.
            let _ = revert_run_id;
            writeln!(
                out,
                "revert\t{}\t{}\t{}\t->\tdeleted",
                target_run_id.0,
                op_mode,
                dest.display(),
            )
            .map_err(|e| format!("write: {e}"))?;
            Ok(ExecOutcome::Reverted)
        }
        RevertStep::Restore { dest, src, op_mode } => {
            if let Some(parent) = src.parent()
                && !parent.as_os_str().is_empty()
                && !parent.exists()
            {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("create_dir_all on source parent: {e}"))?;
            }
            move_or_copy_fallback(dest, src).map_err(|e| format!("restore failed: {e}"))?;
            state
                .delete_placement_for_run(target_run_id, &dest.to_string_lossy())
                .map_err(|e| format!("db delete: {e}"))?;
            writeln!(
                out,
                "revert\t{}\t{}\t{}\t->\trestored to {}",
                target_run_id.0,
                op_mode,
                dest.display(),
                src.display(),
            )
            .map_err(|e| format!("write: {e}"))?;
            Ok(ExecOutcome::Reverted)
        }
        RevertStep::DropRowOnly { dest, op_mode } => {
            state
                .delete_placement_for_run(target_run_id, &dest.to_string_lossy())
                .map_err(|e| format!("db delete: {e}"))?;
            writeln!(
                out,
                "warning\t{}\t{}\t{}\t(dest missing, dropping placement row only)",
                target_run_id.0,
                op_mode,
                dest.display(),
            )
            .map_err(|e| format!("write: {e}"))?;
            Ok(ExecOutcome::Warned)
        }
    }
}

fn print_step<W: Write>(step: &RevertStep, target_run_id: i64, out: &mut W) -> Result<()> {
    match step {
        RevertStep::Delete { dest, op_mode } => writeln!(
            out,
            "revert\t{}\t{}\t{}\t->\twould delete",
            target_run_id,
            op_mode,
            dest.display(),
        )
        .map_err(io_stdout),
        RevertStep::Restore { dest, src, op_mode } => writeln!(
            out,
            "revert\t{}\t{}\t{}\t->\twould restore to {}",
            target_run_id,
            op_mode,
            dest.display(),
            src.display(),
        )
        .map_err(io_stdout),
        RevertStep::DropRowOnly { dest, op_mode } => writeln!(
            out,
            "warning\t{}\t{}\t{}\t(dest missing, would drop placement row only)",
            target_run_id,
            op_mode,
            dest.display(),
        )
        .map_err(io_stdout),
    }
}

fn step_op_mode(step: &RevertStep) -> &str {
    match step {
        RevertStep::Delete { op_mode, .. }
        | RevertStep::Restore { op_mode, .. }
        | RevertStep::DropRowOnly { op_mode, .. } => op_mode,
    }
}

fn step_dest(step: &RevertStep) -> &Path {
    match step {
        RevertStep::Delete { dest, .. }
        | RevertStep::Restore { dest, .. }
        | RevertStep::DropRowOnly { dest, .. } => dest,
    }
}

/// `fs::rename` with copy+remove fallback on EXDEV.
fn move_or_copy_fallback(src: &Path, dst: &Path) -> Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if matches!(e.raw_os_error(), Some(18)) => {
            fs::copy(src, dst).map_err(|source| Error::Apply {
                kind: ApplyErrorKind::Copy,
                path: dst.to_path_buf(),
                source,
            })?;
            fs::remove_file(src).map_err(|source| Error::Apply {
                kind: ApplyErrorKind::Remove,
                path: src.to_path_buf(),
                source,
            })?;
            Ok(())
        }
        Err(source) => Err(Error::Apply {
            kind: ApplyErrorKind::Rename,
            path: dst.to_path_buf(),
            source,
        }),
    }
}

fn io_stdout(source: std::io::Error) -> Error {
    Error::Io {
        path: PathBuf::from("<stdout>"),
        source,
    }
}
