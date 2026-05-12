//! Planner — compose scan, classify, hash, metadata, template, sequence,
//! dedupe, and conflict resolution into a [`Plan`] of actions.
//!
//! Read-only on `placements`: uses [`Sequencer::peek_next`] (not `assign`) so a
//! dry-run never leaves reservation rows behind. `files`/hash cache writes
//! still happen — they describe what's on disk, not what we'd like to do.
//!
//! Candidates are buffered and sorted by `(taken_at ASC, source_path ASC)`
//! before seq assignment so reruns produce identical plans.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use rusqlite::{OptionalExtension, params};

use crate::config::{DedupeScope, OnConflict, OnDuplicate, OpMode, Output, Profile};
use crate::error::{Error, Result};
use crate::hash::{hex, sha256_file};
use crate::kind::classify_scanned;
use crate::metadata::{DateSource, Metadata, extract};
use crate::scan::ScannedFile;
use crate::sequence::Sequencer;
use crate::state::{CachedFileEntry, FileId, PrepareBatch, State, source_path_key, stat_for_cache};
use crate::template::{RenderContext, Template, substitute_seq};

/// Files per BEGIN..COMMIT during the prepare phase. Chosen so a power-loss
/// window doesn't span the entire library, while still amortising fsync across
/// hundreds of file/hash-cache writes.
const PREPARE_BATCH_SIZE: usize = 1000;

/// A plan produced by [`plan`].
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Plan {
    pub actions: Vec<PlannedAction>,
    pub health: Vec<HealthEntry>,
}

/// One planned operation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlannedAction {
    Place {
        src: PathBuf,
        dst: PathBuf,
        mode: OpMode,
        output_name: String,
        file_id: FileId,
        sha256_hex: String,
        seq: u64,
        scope_key: String,
    },
    /// Source is a duplicate of an already-placed file under the active scope.
    SkipDuplicate {
        src: PathBuf,
        existing_dst: PathBuf,
        output_name: String,
    },
    /// `on_conflict = "skip"` and a different file already occupies the path.
    SkipConflict {
        src: PathBuf,
        dst: PathBuf,
        output_name: String,
    },
    /// `on_conflict = "replace"` or `dedupe = "replace"` queued an overwrite.
    Replace {
        src: PathBuf,
        dst: PathBuf,
        mode: OpMode,
        output_name: String,
        file_id: FileId,
        sha256_hex: String,
        seq: u64,
        scope_key: String,
    },
}

/// Non-fatal observation surfaced during planning. The CLI promotes these to
/// a non-zero exit under `--strict`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HealthEntry {
    pub kind: HealthKind,
    pub path: PathBuf,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HealthKind {
    WalkError,
    MissingDate,
    Unclassified,
    Unrouted,
    ExtractFailed,
    HashFailed,
    /// Structural check failed (e.g. JPEG missing FFD9). Surfaced by `health`.
    Truncated,
    /// Recorded sha no longer matches on-disk content.
    Drift,
    /// File present in an output's tree but unknown to the state DB.
    Orphan,
    /// Source file recorded in `files` no longer exists on disk.
    MissingSource,
    /// Placement recorded in `placements` is missing from disk.
    MissingDestination,
}

/// Compose the full pipeline from scan output into a [`Plan`].
pub fn plan(
    state: &mut State,
    profile: &Profile,
    candidates: impl Iterator<Item = Result<ScannedFile>>,
) -> Result<Plan> {
    let mut plan = Plan::default();

    let lower = candidates.size_hint().0;
    let mut scanned: Vec<ScannedFile> = Vec::with_capacity(lower);
    for c in candidates {
        match c {
            Ok(file) => scanned.push(file),
            Err(err) => {
                let path = walker_err_path(&err);
                plan.health.push(HealthEntry {
                    kind: HealthKind::WalkError,
                    path,
                    detail: Some(err.to_string()),
                });
            }
        }
    }

    // Bulk-preload the (source_path → cached digest) index so the parallel
    // pre-compute phase resolves cache hits in memory instead of through a
    // per-file SELECT.
    let cache = state.load_hash_cache()?;

    // Pure-compute phase: stat, hash (or pull from cache), classify, extract
    // metadata. No `&mut State` here — runs across the rayon pool.
    let computed: Vec<std::result::Result<Computed, HealthEntry>> = scanned
        .into_par_iter()
        .map(|file| compute_one(profile, file, &cache))
        .collect();

    let mut ready: Vec<ReadyFile> = Vec::with_capacity(computed.len());
    let mut chunk: Vec<Computed> = Vec::with_capacity(PREPARE_BATCH_SIZE);
    for item in computed {
        match item {
            Ok(c) => {
                chunk.push(c);
                if chunk.len() >= PREPARE_BATCH_SIZE {
                    upsert_chunk(state, &mut chunk, &mut ready, &mut plan)?;
                }
            }
            Err(entry) => plan.health.push(entry),
        }
    }
    if !chunk.is_empty() {
        upsert_chunk(state, &mut chunk, &mut ready, &mut plan)?;
    }

    ready.sort_by(|a, b| {
        a.metadata
            .taken_at
            .cmp(&b.metadata.taken_at)
            .then_with(|| a.file.absolute_path.cmp(&b.file.absolute_path))
    });

    let match_sets = compile_match_sets(&profile.outputs)?;

    // In-batch state: peeked seqs are not reserved, so the planner tracks
    // per-bucket counters and claimed paths locally for the lifetime of this call.
    let mut bucket_next: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut claimed_paths: BTreeSet<(String, PathBuf)> = BTreeSet::new();
    let mut placed_in_batch: BTreeMap<(String, String), PathBuf> = BTreeMap::new();
    let mut placed_in_batch_global: BTreeMap<String, PathBuf> = BTreeMap::new();

    let mut sequencer = Sequencer::new(state, profile);

    for ready_file in ready {
        let any_routed = plan_one(
            &mut sequencer,
            profile,
            &ready_file,
            &match_sets,
            &mut bucket_next,
            &mut claimed_paths,
            &mut placed_in_batch,
            &mut placed_in_batch_global,
            &mut plan,
        )?;

        if !any_routed {
            plan.health.push(HealthEntry {
                kind: HealthKind::Unrouted,
                path: ready_file.file.absolute_path.clone(),
                detail: None,
            });
        }

        if ready_file.metadata.taken_at_source == DateSource::Mtime {
            plan.health.push(HealthEntry {
                kind: HealthKind::MissingDate,
                path: ready_file.file.absolute_path.clone(),
                detail: Some("fell through to mtime".to_string()),
            });
        }
    }

    Ok(plan)
}

struct ReadyFile {
    file: ScannedFile,
    canonical_ext: Option<String>,
    metadata: Metadata,
    sha256_hex: String,
    file_id: FileId,
    classified_as_other: bool,
}

/// Output of the parallel pre-compute phase. Carries everything the DB-write
/// phase needs to finish the row, minus the assigned `FileId`.
struct Computed {
    file: ScannedFile,
    canonical_ext: Option<String>,
    metadata: Metadata,
    sha256: [u8; 32],
    classified_as_other: bool,
}

/// Pure-compute work for a single candidate. No DB access — the cache lookup
/// is in-memory against the bulk-preloaded `cache` map. Errors here are
/// surfaced as [`HealthEntry`]s; nothing here aborts the planner.
fn compute_one(
    profile: &Profile,
    file: ScannedFile,
    cache: &HashMap<String, CachedFileEntry>,
) -> std::result::Result<Computed, HealthEntry> {
    let classified = classify_scanned(profile, &file);

    let sha256 = match resolve_hash(&file, cache) {
        Ok(b) => b,
        Err(e) => {
            return Err(HealthEntry {
                kind: HealthKind::HashFailed,
                path: file.absolute_path.clone(),
                detail: Some(e.to_string()),
            });
        }
    };

    let metadata = match extract(profile, &file, &classified.kind) {
        Ok(m) => m,
        Err(e) => {
            return Err(HealthEntry {
                kind: HealthKind::ExtractFailed,
                path: file.absolute_path.clone(),
                detail: Some(e.to_string()),
            });
        }
    };

    let classified_as_other = classified.kind == crate::kind::OTHER;

    Ok(Computed {
        file,
        canonical_ext: classified.canonical_ext,
        metadata,
        sha256,
        classified_as_other,
    })
}

/// Resolve `file`'s sha256 via the preloaded cache, falling back to a fresh
/// hash on miss. Matches the cache hit semantics of
/// [`State::hash_or_lookup`]: same `(size, mtime_secs, mtime_nanos)` triple,
/// same `source_path` key shape.
fn resolve_hash(file: &ScannedFile, cache: &HashMap<String, CachedFileEntry>) -> Result<[u8; 32]> {
    let path = file.absolute_path.as_path();
    let stat = stat_for_cache(path)?;
    let key = source_path_key(path);
    if let Some(entry) = cache.get(&key)
        && entry.stat == stat
    {
        return Ok(entry.sha256);
    }
    sha256_file(path)
}

/// Drain `chunk` through a single write transaction. Mirrors the old
/// per-file `prepare` write path but amortises the fsync across the batch.
fn upsert_chunk(
    state: &mut State,
    chunk: &mut Vec<Computed>,
    ready: &mut Vec<ReadyFile>,
    plan: &mut Plan,
) -> Result<()> {
    state.with_prepare_tx(|batch: &PrepareBatch<'_>| {
        for c in chunk.drain(..) {
            match batch.upsert_file(&c.file, &c.metadata, c.sha256) {
                Ok(file_id) => ready.push(ReadyFile {
                    file: c.file,
                    canonical_ext: c.canonical_ext,
                    metadata: c.metadata,
                    sha256_hex: hex(&c.sha256),
                    file_id,
                    classified_as_other: c.classified_as_other,
                }),
                Err(e) => plan.health.push(HealthEntry {
                    kind: HealthKind::HashFailed,
                    path: c.file.absolute_path.clone(),
                    detail: Some(e.to_string()),
                }),
            }
        }
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
fn plan_one(
    sequencer: &mut Sequencer<'_>,
    profile: &Profile,
    ready: &ReadyFile,
    match_sets: &BTreeMap<String, GlobSet>,
    bucket_next: &mut BTreeMap<(String, String), u64>,
    claimed_paths: &mut BTreeSet<(String, PathBuf)>,
    placed_in_batch: &mut BTreeMap<(String, String), PathBuf>,
    placed_in_batch_global: &mut BTreeMap<String, PathBuf>,
    plan: &mut Plan,
) -> Result<bool> {
    let mut routed = false;

    if ready.classified_as_other {
        plan.health.push(HealthEntry {
            kind: HealthKind::Unclassified,
            path: ready.file.absolute_path.clone(),
            detail: None,
        });
    }

    for output in &profile.outputs {
        if !output_accepts(output, ready, match_sets) {
            continue;
        }
        routed = true;

        let dir = render_directory(output, ready, &profile.templates.fallbacks)?;
        let scope_key =
            Sequencer::scope_key(profile.sequence.scope, &ready.metadata.taken_at, Some(&dir));

        let bucket = (output.name.clone(), scope_key.clone());
        let seq = match existing_seq_for_file(sequencer.state_mut(), ready.file_id, &output.name)? {
            Some(prior) => prior,
            None => {
                let counter = match bucket_next.get(&bucket).copied() {
                    Some(v) => v,
                    None => sequencer.peek_next(ready.file_id, &output.name, &scope_key)?,
                };
                bucket_next.insert(bucket.clone(), counter.saturating_add(1));
                counter
            }
        };

        let filename = render_filename(output, ready, &profile.templates.fallbacks, Some(seq))?;
        let dst = compose_dst(
            &output.path,
            &dir,
            &filename,
            ready.canonical_ext.as_deref(),
        );

        // Dedupe wins over name conflicts — user intent is "don't double-store
        // the same bytes" regardless of where they'd land.
        let dedupe_hit = check_dedupe(
            sequencer.state_mut(),
            output,
            &profile.dedupe.scope,
            &ready.sha256_hex,
            placed_in_batch,
            placed_in_batch_global,
        )?;
        if let Some(existing_dst) = dedupe_hit {
            match profile.dedupe.on_duplicate {
                OnDuplicate::Skip => {
                    plan.actions.push(PlannedAction::SkipDuplicate {
                        src: ready.file.absolute_path.clone(),
                        existing_dst,
                        output_name: output.name.clone(),
                    });
                    continue;
                }
                OnDuplicate::Replace => {
                    plan.actions.push(PlannedAction::Replace {
                        src: ready.file.absolute_path.clone(),
                        dst: existing_dst.clone(),
                        mode: output.mode,
                        output_name: output.name.clone(),
                        file_id: ready.file_id,
                        sha256_hex: ready.sha256_hex.clone(),
                        seq,
                        scope_key: scope_key.clone(),
                    });
                    track_placed(
                        &output.name,
                        &ready.sha256_hex,
                        &existing_dst,
                        placed_in_batch,
                        placed_in_batch_global,
                    );
                    claimed_paths.insert((output.name.clone(), existing_dst));
                    continue;
                }
                OnDuplicate::KeepBoth => {
                    let resolved = resolve_path_with_keep_both_suffix(
                        sequencer.state_mut(),
                        &dst,
                        &output.name,
                        claimed_paths,
                    )?;
                    let final_dst = resolved.unwrap_or_else(|| dst.clone());
                    place_with_conflict(
                        sequencer.state_mut(),
                        output,
                        ready,
                        seq,
                        &scope_key,
                        &final_dst,
                        claimed_paths,
                        placed_in_batch,
                        placed_in_batch_global,
                        plan,
                    )?;
                    continue;
                }
            }
        }

        place_with_conflict(
            sequencer.state_mut(),
            output,
            ready,
            seq,
            &scope_key,
            &dst,
            claimed_paths,
            placed_in_batch,
            placed_in_batch_global,
            plan,
        )?;
    }

    Ok(routed)
}

#[allow(clippy::too_many_arguments)]
fn place_with_conflict(
    state: &mut State,
    output: &Output,
    ready: &ReadyFile,
    seq: u64,
    scope_key: &str,
    dst: &Path,
    claimed_paths: &mut BTreeSet<(String, PathBuf)>,
    placed_in_batch: &mut BTreeMap<(String, String), PathBuf>,
    placed_in_batch_global: &mut BTreeMap<String, PathBuf>,
    plan: &mut Plan,
) -> Result<()> {
    let conflict = path_is_taken(state, &output.name, dst, claimed_paths)?;

    let chosen_dst: Option<PathBuf> = if !conflict {
        Some(dst.to_path_buf())
    } else {
        match output.on_conflict {
            OnConflict::Skip => {
                plan.actions.push(PlannedAction::SkipConflict {
                    src: ready.file.absolute_path.clone(),
                    dst: dst.to_path_buf(),
                    output_name: output.name.clone(),
                });
                None
            }
            OnConflict::Replace => Some(dst.to_path_buf()),
            OnConflict::Rename => Some(rename_until_free(state, &output.name, dst, claimed_paths)?),
            OnConflict::HashSuffix => Some(hash_suffix_path(
                state,
                &output.name,
                dst,
                &ready.sha256_hex,
                claimed_paths,
            )?),
        }
    };

    let Some(final_dst) = chosen_dst else {
        return Ok(());
    };

    let action = if conflict && matches!(output.on_conflict, OnConflict::Replace) {
        PlannedAction::Replace {
            src: ready.file.absolute_path.clone(),
            dst: final_dst.clone(),
            mode: output.mode,
            output_name: output.name.clone(),
            file_id: ready.file_id,
            sha256_hex: ready.sha256_hex.clone(),
            seq,
            scope_key: scope_key.to_string(),
        }
    } else {
        PlannedAction::Place {
            src: ready.file.absolute_path.clone(),
            dst: final_dst.clone(),
            mode: output.mode,
            output_name: output.name.clone(),
            file_id: ready.file_id,
            sha256_hex: ready.sha256_hex.clone(),
            seq,
            scope_key: scope_key.to_string(),
        }
    };

    plan.actions.push(action);
    claimed_paths.insert((output.name.clone(), final_dst.clone()));
    track_placed(
        &output.name,
        &ready.sha256_hex,
        &final_dst,
        placed_in_batch,
        placed_in_batch_global,
    );
    Ok(())
}

fn output_accepts(
    output: &Output,
    ready: &ReadyFile,
    match_sets: &BTreeMap<String, GlobSet>,
) -> bool {
    if let Some(kinds) = &output.kinds
        && !kinds.iter().any(|k| k == &ready.metadata.kind)
    {
        return false;
    }
    if let Some(set) = match_sets.get(&output.name) {
        let name = ready
            .file
            .absolute_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !set.is_match(&name) {
            return false;
        }
    }
    true
}

fn compile_match_sets(outputs: &[Output]) -> Result<BTreeMap<String, GlobSet>> {
    let mut out = BTreeMap::new();
    for o in outputs {
        let Some(patterns) = &o.match_ else { continue };
        let mut b = GlobSetBuilder::new();
        for p in patterns {
            let g = Glob::new(p).map_err(|source| Error::BadGlob {
                pattern: p.clone(),
                source,
            })?;
            b.add(g);
        }
        let set = b.build().map_err(|source| Error::BadGlob {
            pattern: patterns.join(", "),
            source,
        })?;
        out.insert(o.name.clone(), set);
    }
    Ok(out)
}

fn render_directory(
    output: &Output,
    ready: &ReadyFile,
    fallbacks: &BTreeMap<String, String>,
) -> Result<String> {
    let tpl = output
        .directory_for
        .get(&ready.metadata.kind)
        .unwrap_or(&output.directory);
    let location = format!("output `{}`.directory", output.name);
    render(&location, tpl, ready, fallbacks, None)
}

fn render_filename(
    output: &Output,
    ready: &ReadyFile,
    fallbacks: &BTreeMap<String, String>,
    seq: Option<u64>,
) -> Result<String> {
    let tpl = output
        .filename_for
        .get(&ready.metadata.kind)
        .unwrap_or(&output.filename);
    let location = format!("output `{}`.filename", output.name);
    let rendered = render(&location, tpl, ready, fallbacks, seq)?;
    if let Some(s) = seq {
        substitute_seq(&rendered, s).ok_or_else(|| Error::Template {
            location,
            template: tpl.clone(),
            reason: "malformed seq sentinel produced by render".to_string(),
        })
    } else {
        Ok(rendered)
    }
}

fn render(
    location: &str,
    tpl_str: &str,
    ready: &ReadyFile,
    fallbacks: &BTreeMap<String, String>,
    seq: Option<u64>,
) -> Result<String> {
    let tpl = Template::parse(tpl_str).map_err(|e| Error::Template {
        location: location.to_string(),
        template: tpl_str.to_string(),
        reason: e.to_string(),
    })?;
    let ctx = RenderContext {
        taken_at: &ready.metadata.taken_at,
        metadata: &ready.metadata,
        canonical_ext: ready.canonical_ext.as_deref(),
        sha256_hex: &ready.sha256_hex,
        seq,
        fallbacks,
    };
    tpl.render(&ctx).map_err(|e| Error::Template {
        location: location.to_string(),
        template: tpl_str.to_string(),
        reason: e.to_string(),
    })
}

fn compose_dst(base: &Path, dir: &str, filename: &str, ext: Option<&str>) -> PathBuf {
    let mut p = base.to_path_buf();
    if !dir.is_empty() {
        p.push(dir);
    }
    let final_name = match ext {
        Some(e) if !e.is_empty() => {
            let e = e.strip_prefix('.').unwrap_or(e);
            format!("{filename}.{e}")
        }
        _ => filename.to_string(),
    };
    p.push(final_name);
    p
}

fn check_dedupe(
    state: &mut State,
    output: &Output,
    scope: &DedupeScope,
    sha256_hex: &str,
    placed_in_batch: &BTreeMap<(String, String), PathBuf>,
    placed_in_batch_global: &BTreeMap<String, PathBuf>,
) -> Result<Option<PathBuf>> {
    let in_batch = match scope {
        DedupeScope::Output => placed_in_batch.get(&(output.name.clone(), sha256_hex.to_string())),
        DedupeScope::Global => placed_in_batch_global.get(sha256_hex),
    };
    if let Some(p) = in_batch {
        return Ok(Some(p.clone()));
    }

    let db_path = state.db_path().to_path_buf();
    let conn = state.conn();
    let row: Option<String> = match scope {
        DedupeScope::Output => conn
            .query_row(
                "SELECT p.dest_path \
                 FROM placements p JOIN files f ON p.file_id = f.id \
                 WHERE f.sha256 = ?1 \
                   AND p.output_name = ?2 \
                   AND p.dest_path NOT LIKE ':reserved:%' \
                 LIMIT 1",
                params![sha256_hex, &output.name],
                |r| r.get::<_, String>(0),
            )
            .optional(),
        DedupeScope::Global => conn
            .query_row(
                "SELECT p.dest_path \
                 FROM placements p JOIN files f ON p.file_id = f.id \
                 WHERE f.sha256 = ?1 \
                   AND p.dest_path NOT LIKE ':reserved:%' \
                 LIMIT 1",
                params![sha256_hex],
                |r| r.get::<_, String>(0),
            )
            .optional(),
    }
    .map_err(|e| Error::Sqlite {
        path: db_path,
        source: e,
    })?;

    Ok(row.map(PathBuf::from))
}

fn path_is_taken(
    state: &mut State,
    output_name: &str,
    dst: &Path,
    claimed_paths: &BTreeSet<(String, PathBuf)>,
) -> Result<bool> {
    if claimed_paths.contains(&(output_name.to_string(), dst.to_path_buf())) {
        return Ok(true);
    }
    if dst.exists() {
        return Ok(true);
    }
    let dst_str = dst.to_string_lossy().into_owned();
    let db_path = state.db_path().to_path_buf();
    let conn = state.conn();
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM placements \
             WHERE output_name = ?1 AND dest_path = ?2 \
               AND dest_path NOT LIKE ':reserved:%' \
             LIMIT 1",
            params![output_name, &dst_str],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(|e| Error::Sqlite {
            path: db_path,
            source: e,
        })?;
    Ok(exists.is_some())
}

fn existing_seq_for_file(
    state: &mut State,
    file_id: FileId,
    output_name: &str,
) -> Result<Option<u64>> {
    let db_path = state.db_path().to_path_buf();
    let conn = state.conn();
    let seq: Option<i64> = conn
        .query_row(
            "SELECT seq FROM placements \
             WHERE file_id = ?1 AND output_name = ?2 AND seq IS NOT NULL \
             LIMIT 1",
            params![file_id.0, output_name],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(|e| Error::Sqlite {
            path: db_path,
            source: e,
        })?;
    Ok(seq.map(|v| u64::try_from(v).unwrap_or(0)))
}

/// Append `_2`, `_3`, … to the stem until free. Caps at 9999 as a defensive
/// upper bound.
fn rename_until_free(
    state: &mut State,
    output_name: &str,
    dst: &Path,
    claimed_paths: &BTreeSet<(String, PathBuf)>,
) -> Result<PathBuf> {
    for n in 2..=9999u32 {
        let candidate = with_stem_suffix(dst, &format!("_{n}"));
        if !path_is_taken(state, output_name, &candidate, claimed_paths)? {
            return Ok(candidate);
        }
    }
    Ok(with_stem_suffix(dst, "_overflow"))
}

fn resolve_path_with_keep_both_suffix(
    state: &mut State,
    dst: &Path,
    output_name: &str,
    claimed_paths: &BTreeSet<(String, PathBuf)>,
) -> Result<Option<PathBuf>> {
    for n in 1..=9999u32 {
        let candidate = with_stem_suffix(dst, &format!("_dup{n}"));
        if !path_is_taken(state, output_name, &candidate, claimed_paths)? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn hash_suffix_path(
    state: &mut State,
    output_name: &str,
    dst: &Path,
    sha256_hex: &str,
    claimed_paths: &BTreeSet<(String, PathBuf)>,
) -> Result<PathBuf> {
    let short = &sha256_hex[..sha256_hex.len().min(8)];
    let candidate = with_stem_suffix(dst, &format!("_{short}"));
    if !path_is_taken(state, output_name, &candidate, claimed_paths)? {
        return Ok(candidate);
    }
    rename_until_free(state, output_name, &candidate, claimed_paths)
}

/// Insert `suffix` between the file stem and its extension. Dotfiles (no real
/// stem) get the suffix appended to the whole name.
fn with_stem_suffix(path: &Path, suffix: &str) -> PathBuf {
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    let Some(name) = path.file_name() else {
        return path.to_path_buf();
    };
    let name = name.to_string_lossy();
    let new_name = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}{suffix}.{ext}"),
        _ => format!("{name}{suffix}"),
    };
    parent.join(new_name)
}

fn track_placed(
    output_name: &str,
    sha256_hex: &str,
    dst: &Path,
    placed_in_batch: &mut BTreeMap<(String, String), PathBuf>,
    placed_in_batch_global: &mut BTreeMap<String, PathBuf>,
) {
    placed_in_batch
        .entry((output_name.to_string(), sha256_hex.to_string()))
        .or_insert_with(|| dst.to_path_buf());
    placed_in_batch_global
        .entry(sha256_hex.to_string())
        .or_insert_with(|| dst.to_path_buf());
}

fn walker_err_path(err: &Error) -> PathBuf {
    match err {
        Error::WalkDir { path, .. }
        | Error::PathStripPrefix { path, .. }
        | Error::Io { path, .. } => path.clone(),
        _ => PathBuf::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::ScannedFile;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn with_stem_suffix_keeps_extension() {
        let r = with_stem_suffix(&p("/tmp/a/file.jpg"), "_2");
        assert_eq!(r, p("/tmp/a/file_2.jpg"));
    }

    #[test]
    fn with_stem_suffix_no_extension() {
        let r = with_stem_suffix(&p("/tmp/a/README"), "_2");
        assert_eq!(r, p("/tmp/a/README_2"));
    }

    #[test]
    fn with_stem_suffix_dotfile_keeps_dot() {
        let r = with_stem_suffix(&p("/tmp/.hidden"), "_2");
        assert_eq!(r, p("/tmp/.hidden_2"));
    }

    #[test]
    fn compose_dst_appends_extension() {
        let d = compose_dst(&p("/out"), "2024/03", "2024-03-15_00001", Some("jpg"));
        assert_eq!(d, p("/out/2024/03/2024-03-15_00001.jpg"));
    }

    #[test]
    fn compose_dst_handles_leading_dot_in_ext() {
        let d = compose_dst(&p("/out"), "", "name", Some(".jpg"));
        assert_eq!(d, p("/out/name.jpg"));
    }

    #[test]
    fn compose_dst_no_extension() {
        let d = compose_dst(&p("/out"), "2024", "README", None);
        assert_eq!(d, p("/out/2024/README"));
    }

    #[test]
    fn output_accepts_kinds_filter() {
        let scanned = ScannedFile {
            source_root: p("/in"),
            absolute_path: p("/in/a.jpg"),
            relative_path: p("a.jpg"),
        };
        let metadata = Metadata {
            taken_at: chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            taken_at_source: DateSource::Exif,
            camera: None,
            lens: None,
            kind: "photo".to_string(),
            width: None,
            height: None,
            author: None,
            title: None,
            vendor: None,
        };
        let ready = ReadyFile {
            file: scanned,
            canonical_ext: Some("jpg".to_string()),
            metadata,
            sha256_hex: "0".repeat(64),
            file_id: FileId(1),
            classified_as_other: false,
        };
        let mut output: Output = toml::from_str(
            r#"name = "lib"
path = "/o"
directory = "{yyyy}"
filename = "{yyyy}"
kinds = ["video"]
"#,
        )
        .unwrap();
        assert!(!output_accepts(&output, &ready, &BTreeMap::new()));
        output.kinds = Some(vec!["photo".to_string()]);
        assert!(output_accepts(&output, &ready, &BTreeMap::new()));
        output.kinds = None;
        assert!(output_accepts(&output, &ready, &BTreeMap::new()));
    }
}
