//! End-to-end glue used by the CLI for `shelf run`, `plan`, `status`,
//! `health`, `verify`, `runs`, and `revert`.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::apply::{ApplyReport, apply};
use crate::config::{Profile, ProfileEntry, discover_profiles, load_profile, resolve_profile};
use crate::error::{Error, Result};
use crate::health::{HealthReport, check as run_health_checks};
use crate::plan::{HealthEntry, HealthKind, Plan, PlannedAction, plan};
use crate::progress::Progress;
use crate::revert::{RevertOptions, RevertOutcome, RevertRefusal, precheck, revert};
use crate::scan::{scan, scan_profile};
use crate::state::{RunCounts, RunId, RunRow, State};
use crate::verify::{Mode as VerifyMode, VerifyReport, run as run_verify};

/// Outcome of [`run`]. The CLI consumes this to pick an exit code.
#[derive(Debug)]
pub struct RunOutcome {
    pub plan: Plan,
    pub apply_report: Option<ApplyReport>,
    pub dry_run: bool,
    pub run_id: Option<RunId>,
}

impl RunOutcome {
    #[must_use]
    pub fn has_health_entries(&self) -> bool {
        !self.plan.health.is_empty()
    }
}

/// Outcome of [`health`].
#[derive(Debug)]
pub struct HealthOutcome {
    pub report: HealthReport,
}

impl HealthOutcome {
    #[must_use]
    pub fn has_entries(&self) -> bool {
        !self.report.is_empty()
    }
}

/// Outcome of [`verify`].
#[derive(Debug)]
pub struct VerifyOutcome {
    pub report: VerifyReport,
}

impl VerifyOutcome {
    #[must_use]
    pub fn has_entries(&self) -> bool {
        !self.report.is_empty()
    }
}

/// Drive the full pipeline for one profile.
///
/// When `from_overrides` is non-empty, those paths replace the profile's
/// `inputs` for the scan step (canonicalized to absolute first).
///
/// Opens a `runs` row at start and finalises on completion. Mid-run crashes
/// leave the row visible as `(incomplete)`.
#[allow(clippy::too_many_arguments)]
pub fn run<W: Write, E: Write>(
    profile_name: Option<&str>,
    config_override: Option<&Path>,
    from_overrides: &[PathBuf],
    dry_run: bool,
    strict: bool,
    show_all: bool,
    out: &mut W,
    err: &mut E,
) -> Result<RunOutcome> {
    let (resolved_name, profile_path, profile) = load(profile_name, config_override)?;
    tracing::debug!(
        profile = %resolved_name,
        path = %profile_path.display(),
        from_count = from_overrides.len(),
        dry_run,
        "run start"
    );

    let mut state = State::open(&resolved_name, &profile)?;
    let run_id = state.open_run(&resolved_name, from_overrides, dry_run, strict)?;

    let progress = Progress::stderr();

    let plan = if from_overrides.is_empty() {
        let candidates = scan_profile(&profile)?;
        plan(&mut state, &profile, candidates, Some(&progress))?
    } else {
        let roots = canonicalize_overrides(from_overrides)?;
        tracing::debug!(roots = ?roots, "scan: --from override");
        let candidates = scan(&roots, &profile.filters)?;
        plan(&mut state, &profile, candidates, Some(&progress))?
    };

    progress.done();
    print_plan(&plan, show_all, out)?;

    if dry_run {
        write_summary_dry_run(&plan, err)?;
        let counts = RunCounts {
            health: plan.health.len() as u64,
            ..RunCounts::default()
        };
        state.finish_run(run_id, &counts)?;
        return Ok(RunOutcome {
            plan,
            apply_report: None,
            dry_run: true,
            run_id: Some(run_id),
        });
    }

    let progress = Progress::stderr();
    let report = apply(&mut state, &profile, &plan, Some(run_id), Some(&progress))?;
    progress.done();
    write_summary_apply(&plan, &report, err)?;

    let counts = RunCounts {
        placed: report.placed,
        replaced: report.replaced,
        skipped_duplicate: report.skipped_duplicate,
        skipped_conflict: report.skipped_conflict,
        failed: report.failed.len() as u64,
        health: plan.health.len() as u64,
    };
    state.finish_run(run_id, &counts)?;

    Ok(RunOutcome {
        plan,
        apply_report: Some(report),
        dry_run: false,
        run_id: Some(run_id),
    })
}

/// Read-only health diagnostics for one profile.
pub fn health<W: Write, E: Write>(
    profile_name: Option<&str>,
    config_override: Option<&Path>,
    sample: usize,
    out: &mut W,
    err: &mut E,
) -> Result<HealthOutcome> {
    let (resolved_name, profile_path, profile) = load(profile_name, config_override)?;
    tracing::debug!(
        profile = %resolved_name,
        path = %profile_path.display(),
        sample,
        "health start"
    );

    // Read-only open; absence of DB is a clean no-op so a freshly-installed
    // profile doesn't fail health.
    let state = match State::open_readonly(&resolved_name, &profile) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "health: no state DB; skipping DB-backed checks");
            writeln!(
                err,
                "health: no state DB yet for `{resolved_name}`; nothing to report"
            )
            .map_err(io_stderr)?;
            return Ok(HealthOutcome {
                report: HealthReport::default(),
            });
        }
    };

    let report = run_health_checks(&profile, &state, sample)?;
    print_health(&report, out)?;
    writeln!(err, "health: {} entries", report.len()).map_err(io_stderr)?;

    Ok(HealthOutcome { report })
}

/// Verify placement integrity. Rehashes destination-side placements and
/// writes drift/missing-destination entries to the `health` table.
pub fn verify<W: Write, E: Write>(
    profile_name: Option<&str>,
    config_override: Option<&Path>,
    full: bool,
    sample_override: Option<usize>,
    out: &mut W,
    err: &mut E,
) -> Result<VerifyOutcome> {
    let (resolved_name, profile_path, profile) = load(profile_name, config_override)?;
    tracing::debug!(
        profile = %resolved_name,
        path = %profile_path.display(),
        full,
        sample = ?sample_override,
        "verify start"
    );

    // Verify needs write access for the `health` table.
    let mut state = match State::open(&resolved_name, &profile) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "verify: no state DB; nothing to verify");
            writeln!(
                err,
                "verify: no state DB yet for `{resolved_name}`; nothing to verify"
            )
            .map_err(io_stderr)?;
            return Ok(VerifyOutcome {
                report: VerifyReport::default(),
            });
        }
    };

    let mode = if full {
        VerifyMode::Full
    } else {
        let n = match sample_override {
            Some(n) => n,
            None => {
                let total: i64 = state
                    .conn()
                    .query_row(
                        "SELECT COUNT(*) FROM placements \
                         WHERE dest_path NOT LIKE ':reserved:%'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                let total_usize = usize::try_from(total).unwrap_or(0);
                VerifyMode::default_sample_count(total_usize)
            }
        };
        VerifyMode::Sample(n)
    };

    let report = run_verify(&mut state, mode)?;
    print_verify(&report, out)?;
    writeln!(
        err,
        "verify: checked {}/{} placements, {} entries",
        report.checked,
        report.total_placements,
        report.len()
    )
    .map_err(io_stderr)?;

    Ok(VerifyOutcome { report })
}

/// One row per profile written to `out`, sorted by name.
pub fn status<W: Write>(config_override: Option<&Path>, out: &mut W) -> Result<()> {
    let entries = list_profiles(config_override)?;

    writeln!(out, "profile\tfiles\tplacements\tbytes\tlast_run").map_err(io_stdout)?;

    for entry in entries {
        let row = match read_status_row(&entry) {
            Ok(row) => row,
            Err(e) => {
                tracing::warn!(
                    profile = %entry.name,
                    error = %e,
                    "status: skipping profile (db unavailable)"
                );
                StatusRow {
                    files: None,
                    placements: None,
                    bytes: None,
                    last_run: None,
                }
            }
        };
        writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}",
            entry.name,
            row.files
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            row.placements
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            row.bytes
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            row.last_run.as_deref().unwrap_or("-"),
        )
        .map_err(io_stdout)?;
    }

    Ok(())
}

/// `shelf runs [profile] [--limit N]` — list runs newest-first.
pub fn runs_list<W: Write>(
    profile_name: Option<&str>,
    config_override: Option<&Path>,
    limit: usize,
    out: &mut W,
) -> Result<()> {
    let (resolved_name, _profile_path, profile) = load(profile_name, config_override)?;
    let state = State::open_readonly(&resolved_name, &profile)?;
    let rows = state.list_runs(Some(&resolved_name), limit)?;

    writeln!(
        out,
        "id\tstarted_at\tprofile\tkind\tdry_run\tplaced\treplaced\tskipped\tfailed\tstatus"
    )
    .map_err(io_stdout)?;

    for r in &rows {
        let dry_run = if r.dry_run { "yes" } else { "-" };
        let skipped = r.counts.skipped_duplicate + r.counts.skipped_conflict;
        let status = run_status(r);
        writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            r.id.0,
            r.started_at,
            r.profile,
            r.kind,
            dry_run,
            r.counts.placed,
            r.counts.replaced,
            skipped,
            r.counts.failed,
            status,
        )
        .map_err(io_stdout)?;
    }
    Ok(())
}

/// `shelf runs <id>` — list the placements for one run.
pub fn runs_show<W: Write>(
    profile_name: Option<&str>,
    config_override: Option<&Path>,
    run_id: i64,
    out: &mut W,
) -> Result<()> {
    let (resolved_name, _profile_path, profile) = load(profile_name, config_override)?;
    let state = State::open_readonly(&resolved_name, &profile)?;
    let placements = state.placements_for_run(RunId(run_id))?;

    writeln!(out, "file_id\top_mode\tsrc\tdst\tseq").map_err(io_stdout)?;
    for p in &placements {
        let seq = p
            .seq
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string());
        writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}",
            p.file_id,
            p.op_mode.as_deref().unwrap_or("-"),
            p.source_path.display(),
            p.dest_path.display(),
            seq,
        )
        .map_err(io_stdout)?;
    }
    Ok(())
}

/// `shelf revert <id> [--dry-run] [--force]`.
pub fn revert_run<W: Write, E: Write>(
    profile_name: Option<&str>,
    config_override: Option<&Path>,
    target_run_id: i64,
    dry_run: bool,
    force: bool,
    out: &mut W,
    err: &mut E,
) -> Result<RevertOutcome> {
    let (resolved_name, _profile_path, profile) = load(profile_name, config_override)?;
    let mut state = State::open(&resolved_name, &profile)?;

    match precheck(&state, RunId(target_run_id), force)? {
        Err(RevertRefusal::NotFound) => {
            let msg = format!("run {target_run_id} not found");
            writeln!(err, "revert: {msg}").map_err(io_stderr)?;
            return Err(Error::RevertRefused(msg));
        }
        Err(RevertRefusal::DryRunTarget) => {
            let msg = format!("run {target_run_id} was a dry-run; nothing to revert");
            writeln!(err, "revert: {msg}").map_err(io_stderr)?;
            return Err(Error::RevertRefused(msg));
        }
        Err(RevertRefusal::RevertKindTarget) => {
            let msg = format!("run {target_run_id} is itself a revert; refusing");
            writeln!(err, "revert: {msg}").map_err(io_stderr)?;
            return Err(Error::RevertRefused(msg));
        }
        Err(RevertRefusal::AlreadyReverted { reverted_by }) => {
            let by = reverted_by.map(|b| format!(" by {b}")).unwrap_or_default();
            let msg =
                format!("run {target_run_id} was already reverted{by}; use --force to re-revert");
            writeln!(err, "revert: {msg}").map_err(io_stderr)?;
            return Err(Error::RevertRefused(msg));
        }
        Ok(_row) => {}
    }

    let options = RevertOptions { force, dry_run };
    let outcome = revert(
        &mut state,
        &resolved_name,
        RunId(target_run_id),
        options,
        out,
    )?;

    writeln!(
        err,
        "revert: {} reverted, {} warnings, {} failed",
        outcome.report.reverted,
        outcome.report.warnings,
        outcome.report.failed.len(),
    )
    .map_err(io_stderr)?;

    Ok(outcome)
}

/// Write the plan in the stable line-per-action format. When `show_all` is
/// false, suppresses `SkipDuplicate`, `SkipConflict`, and `MissingDate`.
pub fn print_plan<W: Write>(plan: &Plan, show_all: bool, out: &mut W) -> Result<()> {
    for action in &plan.actions {
        match action {
            PlannedAction::Place {
                src,
                dst,
                output_name,
                seq,
                ..
            } => writeln!(
                out,
                "place\t{}\t{}\t{}\tseq={}",
                output_name,
                src.display(),
                dst.display(),
                seq
            )
            .map_err(io_stdout)?,
            PlannedAction::Replace {
                src,
                dst,
                output_name,
                seq,
                ..
            } => writeln!(
                out,
                "replace\t{}\t{}\t{}\tseq={}",
                output_name,
                src.display(),
                dst.display(),
                seq
            )
            .map_err(io_stdout)?,
            PlannedAction::SkipDuplicate {
                src,
                existing_dst,
                output_name,
            } if show_all => writeln!(
                out,
                "skip-duplicate\t{}\t{}\texisting={}",
                output_name,
                src.display(),
                existing_dst.display()
            )
            .map_err(io_stdout)?,
            PlannedAction::SkipConflict {
                src,
                dst,
                output_name,
            } if show_all => writeln!(
                out,
                "skip-conflict\t{}\t{}\tdst={}",
                output_name,
                src.display(),
                dst.display()
            )
            .map_err(io_stdout)?,
            PlannedAction::SkipDuplicate { .. } | PlannedAction::SkipConflict { .. } => {}
        }
    }
    for h in &plan.health {
        if !show_all && h.kind == HealthKind::MissingDate {
            continue;
        }
        writeln!(
            out,
            "health: {}\t{}",
            health_kind_str(h.kind),
            h.path.display()
        )
        .map_err(io_stdout)?;
    }
    Ok(())
}

pub fn print_health<W: Write>(report: &HealthReport, out: &mut W) -> Result<()> {
    for h in &report.entries {
        write_health_entry(h, out)?;
    }
    Ok(())
}

pub fn print_verify<W: Write>(report: &VerifyReport, out: &mut W) -> Result<()> {
    for h in &report.entries {
        write_health_entry(h, out)?;
    }
    Ok(())
}

struct StatusRow {
    files: Option<i64>,
    placements: Option<i64>,
    bytes: Option<i64>,
    last_run: Option<String>,
}

fn run_status(r: &RunRow) -> String {
    if r.finished_at.is_none() {
        return "(incomplete)".to_string();
    }
    if r.dry_run {
        return "(dry-run)".to_string();
    }
    if let Some(by) = r.reverted_by {
        return format!("reverted by {}", by.0);
    }
    "(none)".to_string()
}

fn write_health_entry<W: Write>(h: &HealthEntry, out: &mut W) -> Result<()> {
    match h.kind {
        HealthKind::Drift => writeln!(
            out,
            "health: {}\t{}\t{}",
            health_kind_str(h.kind),
            h.path.display(),
            h.detail.as_deref().unwrap_or("")
        )
        .map_err(io_stdout),
        _ => writeln!(
            out,
            "health: {}\t{}",
            health_kind_str(h.kind),
            h.path.display()
        )
        .map_err(io_stdout),
    }
}

fn load(
    profile_name: Option<&str>,
    config_override: Option<&Path>,
) -> Result<(String, PathBuf, Profile)> {
    let path = resolve_profile(profile_name, config_override)?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| "profile".to_owned());
    let profile = load_profile(&path)?;
    Ok((name, path, profile))
}

fn canonicalize_overrides(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let abs = p.canonicalize().map_err(|source| Error::Io {
            path: p.clone(),
            source,
        })?;
        out.push(abs);
    }
    Ok(out)
}

fn list_profiles(config_override: Option<&Path>) -> Result<Vec<ProfileEntry>> {
    if let Some(path) = config_override {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| "profile".to_owned());
        return Ok(vec![ProfileEntry {
            name,
            path: path.to_path_buf(),
        }]);
    }
    let dir = crate::config::default_config_dir()?;
    discover_profiles(&dir)
}

fn read_status_row(entry: &ProfileEntry) -> Result<StatusRow> {
    let profile = load_profile(&entry.path)?;
    let state = match State::open_readonly(&entry.name, &profile) {
        Ok(s) => s,
        Err(_) => {
            return Ok(StatusRow {
                files: None,
                placements: None,
                bytes: None,
                last_run: None,
            });
        }
    };

    let conn = state.conn();
    let files: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap_or(0);
    let placements: i64 = conn
        .query_row("SELECT COUNT(*) FROM placements", [], |r| r.get(0))
        .unwrap_or(0);
    let bytes: i64 = conn
        .query_row("SELECT COALESCE(SUM(size), 0) FROM files", [], |r| r.get(0))
        .unwrap_or(0);
    let last_run: Option<String> = conn
        .query_row("SELECT MAX(last_seen) FROM files", [], |r| {
            r.get::<_, Option<String>>(0)
        })
        .unwrap_or(None);

    Ok(StatusRow {
        files: Some(files),
        placements: Some(placements),
        bytes: Some(bytes),
        last_run,
    })
}

pub(crate) fn health_kind_str(kind: HealthKind) -> &'static str {
    match kind {
        HealthKind::WalkError => "walk-error",
        HealthKind::MissingDate => "missing-date",
        HealthKind::Unclassified => "unclassified",
        HealthKind::Unrouted => "unrouted",
        HealthKind::ExtractFailed => "extract-failed",
        HealthKind::HashFailed => "hash-failed",
        HealthKind::Truncated => "truncated",
        HealthKind::Drift => "drift",
        HealthKind::Orphan => "orphan",
        HealthKind::MissingSource => "missing-source",
        HealthKind::MissingDestination => "missing-destination",
    }
}

fn write_summary_dry_run<E: Write>(plan: &Plan, err: &mut E) -> Result<()> {
    let (would_place, would_skip, would_conflict) = tally(&plan.actions);
    writeln!(
        err,
        "dry-run: {would_place} would place, {would_skip} would skip, \
         {would_conflict} conflict, {health} health",
        health = plan.health.len(),
    )
    .map_err(io_stderr)
}

fn write_summary_apply<E: Write>(plan: &Plan, report: &ApplyReport, err: &mut E) -> Result<()> {
    writeln!(
        err,
        "apply: {placed} placed, {replaced} replaced, {sd} skipped-dup, \
         {sc} skipped-conflict, {failed} failed, {health} health",
        placed = report.placed,
        replaced = report.replaced,
        sd = report.skipped_duplicate,
        sc = report.skipped_conflict,
        failed = report.failed.len(),
        health = plan.health.len(),
    )
    .map_err(io_stderr)
}

fn tally(actions: &[PlannedAction]) -> (u64, u64, u64) {
    let mut place = 0_u64;
    let mut skip = 0_u64;
    let mut conflict = 0_u64;
    for a in actions {
        match a {
            PlannedAction::Place { .. } | PlannedAction::Replace { .. } => place += 1,
            PlannedAction::SkipDuplicate { .. } => skip += 1,
            PlannedAction::SkipConflict { .. } => conflict += 1,
        }
    }
    (place, skip, conflict)
}

fn io_stdout(source: std::io::Error) -> Error {
    Error::Io {
        path: PathBuf::from("<stdout>"),
        source,
    }
}

fn io_stderr(source: std::io::Error) -> Error {
    Error::Io {
        path: PathBuf::from("<stderr>"),
        source,
    }
}

#[must_use]
pub fn count_health(entries: &[HealthEntry]) -> usize {
    entries.len()
}
