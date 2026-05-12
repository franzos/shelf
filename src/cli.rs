use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};

const TOP_LEVEL_LONG_ABOUT: &str = "\
Catalogue files by metadata-driven rules.

`shelf` walks one or more input directories, extracts metadata (EXIF, \
QuickTime, PDF info dict, filename patterns, mtime), and routes each file \
into one or more outputs whose layout is described by templates. State is \
kept in a per-profile SQLite DB so reruns are cheap and idempotent.

EXIT CODES
    0   success.
    1   runtime error (io, sqlite, hash, template render, ...).
    2   structural error: profile not found, validation failed,
        unimplemented subcommand, or clap argument mismatch.
    3   --strict: planning surfaced one or more health entries.
    4   `shelf health` or `shelf verify` reported one or more entries.

EXAMPLES
    # List every profile shelf can see.
    shelf list

    # Plan (dry-run) the default profile.
    shelf plan

    # Apply the photos profile, failing CI if anything looks off.
    shelf run photos --strict

    # Walk an ad-hoc directory instead of the profile's `inputs`.
    shelf plan photos --from /tmp/new-dump

    # Report library health without writing.
    shelf health photos

    # See past runs, then undo one.
    shelf runs photos
    shelf revert 42
";

#[derive(Debug, Parser)]
#[command(
    name = "shelf",
    version,
    about = "Catalogue files by metadata-driven rules",
    long_about = TOP_LEVEL_LONG_ABOUT,
)]
pub struct Cli {
    /// Increase log verbosity. Repeat for more (`-v`, `-vv`).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Path to a specific profile TOML. Bypasses the discovery directory.
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// Plan only — do not modify the filesystem or write `placements` rows.
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Promote health entries to a non-zero exit code (3).
    #[arg(long, global = true)]
    pub strict: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Args)]
pub struct ProfileArgs {
    /// Profile name. Optional when exactly one profile exists.
    pub profile: Option<String>,
}

/// Args for `shelf run` and `shelf plan`. `--from <PATH>` is repeatable and
/// replaces the profile's `inputs` when given; paths are canonicalized at
/// runtime so relative paths resolve against the cwd.
#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    /// Profile name. Optional when exactly one profile exists.
    pub profile: Option<String>,

    /// Ad-hoc input root, overriding the profile's `inputs`. Repeatable.
    #[arg(long = "from", value_name = "PATH", action = ArgAction::Append)]
    pub from: Vec<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct HealthArgs {
    /// Profile name.
    pub profile: Option<String>,
    /// How many recent `files` rows to spot-check for hash drift. `0` skips.
    #[arg(long, value_name = "N")]
    pub sample: Option<usize>,
}

/// Args for `shelf verify`. `--full` and `--sample` are mutually exclusive.
#[derive(Debug, Clone, Args)]
pub struct VerifyArgs {
    /// Profile name.
    pub profile: Option<String>,
    /// Rehash every placement. Mutually exclusive with `--sample`.
    #[arg(long, conflicts_with = "sample")]
    pub full: bool,
    /// Rehash N randomly selected placements. `0` skips.
    #[arg(long, value_name = "N", conflicts_with = "full")]
    pub sample: Option<usize>,
}

/// Args for `shelf runs`. Positional disambiguation:
/// - `shelf runs` — list runs of the default profile.
/// - `shelf runs photos` — list runs of `photos`.
/// - `shelf runs photos 42` — show placements for run 42 of `photos`.
/// - `shelf runs 42` — show placements for run 42 of the default profile.
#[derive(Debug, Clone, Args)]
pub struct RunsArgs {
    /// Profile name, or — when given alone and numeric — the run id.
    pub profile_or_id: Option<String>,
    /// Run id when `profile_or_id` was a profile name.
    pub id: Option<i64>,
    /// Cap the list to N rows.
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub limit: usize,
}

/// Args for `shelf revert`. Mirrors [`RunsArgs`]: one or two positionals,
/// disambiguated at runtime.
#[derive(Debug, Clone, Args)]
pub struct RevertArgs {
    /// Profile name, or — when given alone and numeric — the run id.
    pub profile_or_id: String,
    /// Run id when `profile_or_id` was a profile name.
    pub id: Option<i64>,
    /// Override drift detection and the move-revert "source already
    /// exists" check. **Destructive** — can overwrite user data.
    #[arg(long)]
    pub force: bool,
}

/// Subcommand surface. Read-only commands (`status`, `list`, `health`,
/// `runs`) ignore the global `--dry-run` flag.
///
/// ## Plan output format (stable)
///
/// `shelf plan` and `shelf run --dry-run` emit one tab-separated line per
/// planned action, plus one tab-separated line per health entry:
///
/// ```text
/// place\t<output>\t<src>\t<dst>\tseq=<N>
/// replace\t<output>\t<src>\t<dst>\tseq=<N>
/// skip-duplicate\t<output>\t<src>\texisting=<dst>
/// skip-conflict\t<output>\t<src>\tdst=<dst>
/// health: unrouted\t<path>
/// health: missing-date\t<path>
/// health: unclassified\t<path>
/// health: walk-error\t<path>
/// health: extract-failed\t<path>
/// health: hash-failed\t<path>
/// ```
///
/// A one-line summary follows on stderr:
///
/// ```text
/// dry-run: 42 would place, 3 would skip, 0 conflict, 0 errors
/// apply:   42 placed, 0 replaced, 3 skipped, 0 conflict, 0 errors
/// ```
///
/// ## `shelf health` output format
///
/// One tab-separated `health: <kind>\t<path>` line per finding. Drift
/// lines carry an `expected=<8hex> got=<8hex>` detail tail. Exit code 4
/// if any entry is reported.
///
/// ## `shelf verify` output format
///
/// Same as `shelf health` for the kinds it produces (`drift`,
/// `missing-destination`). Drift entries are also persisted to the
/// `health` table.
///
/// ## `shelf runs` output format
///
/// List mode:
///
/// ```text
/// id\tstarted_at\tprofile\tkind\tdry_run\tplaced\treplaced\tskipped\tfailed\tstatus
/// ```
///
/// Show mode:
///
/// ```text
/// file_id\top_mode\tsrc\tdst\tseq
/// ```
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Walk inputs, plan placements, perform file operations.
    #[command(long_about = "\
Walk the inputs of a profile, compute the placement plan, and execute the \
file operations (copy / move / hardlink / symlink). State is updated \
atomically per file: a successful operation produces exactly one new row \
in `placements`.

Use --dry-run to see what would happen without touching disk (this is also \
what `shelf plan` does). Use --strict in CI to fail on any health entry. \
Pass --from <PATH> (repeatable) to walk an ad-hoc directory instead of the \
profile's `inputs`; filters, dedupe, and sequence numbering still apply.

EXAMPLES
    shelf run                          # default profile
    shelf run photos                   # named profile
    shelf run photos --dry-run         # like `shelf plan photos`
    shelf run photos --strict          # fail on health entries
    shelf run photos --from /tmp/dump  # override profile inputs
    shelf run --config ./alt.toml      # ad-hoc profile path
")]
    Run(RunArgs),

    /// Alias for `run --dry-run`. Prints the plan without writing.
    #[command(long_about = "\
Show the placement plan without touching the filesystem or writing to the \
`placements` table. Equivalent to `shelf run [profile] --dry-run`.

Output is stable, tab-separated, and suitable for diffing across runs (see \
`shelf --help` for the full action grammar). Like `run`, accepts repeatable \
--from <PATH> overrides to scan an ad-hoc directory instead of the profile's \
`inputs`.

EXAMPLES
    shelf plan
    shelf plan photos
    shelf plan photos --from /tmp/dump
    shelf plan photos | diff -u prev.plan -
")]
    Plan(RunArgs),

    /// Report library health (truncated files, missing metadata, drift, ...).
    #[command(long_about = "\
Read-only health report: surfaces truncated files, files with missing \
date metadata, sha256 drift, orphans in output trees, unclassified files, \
and unrouted files. Exits 4 if any entry is reported, 0 otherwise.

The `--sample N` flag controls how many recent `files` rows are spot-checked \
for hash drift. Set to 0 to skip drift entirely; the default is documented \
on `shelf::health::DEFAULT_SAMPLE`.

EXAMPLES
    shelf health
    shelf health photos
    shelf health photos --sample 0     # skip drift check
    shelf health photos --sample 500   # broader spot-check
")]
    Health(HealthArgs),

    /// Rehash placements and flag drift on the destination side.
    #[command(long_about = "\
Rehash placement destinations and write any drift to the `health` table. \
The default mode samples ~1% of placements (floor 1); use --full to rehash \
everything, or --sample N to pick a specific count. Exits 4 if any drift or \
missing-destination entry is reported, 0 otherwise.

EXAMPLES
    shelf verify                       # sample mode, default profile
    shelf verify photos                # sample mode, named profile
    shelf verify photos --full         # rehash every placement
    shelf verify photos --sample 100   # rehash 100 random placements
")]
    Verify(VerifyArgs),

    /// Summarize every known profile and its state DB.
    #[command(long_about = "\
Summarize every profile shelf can see along with a quick view of its \
state DB: file count, placement count, and pending health entries.

EXAMPLES
    shelf status
")]
    Status,

    /// List profiles in the discovery directory.
    #[command(long_about = "\
List every profile in the discovery directory, one per line, as \
`<name>\\t<path>` for easy machine consumption.

EXAMPLES
    shelf list
    shelf list | awk '{print $1}'      # just the names
")]
    List,

    /// List past runs or show one run's placements.
    #[command(long_about = "\
List runs from the state DB, newest first. With a numeric argument, show \
the placements that one run produced.

EXAMPLES
    shelf runs                         # list runs of the default profile
    shelf runs photos                  # list runs of `photos`
    shelf runs photos 42               # placements for run 42 of `photos`
    shelf runs 42                      # placements for run 42 (default profile)
    shelf runs --limit 5               # only the 5 most recent
")]
    Runs(RunsArgs),

    /// Undo a prior `shelf run`.
    #[command(long_about = "\
Revert the placements a prior run produced. The target run is identified \
by id (see `shelf runs`). Each placement is undone according to its op \
mode:

  copy / hardlink / symlink  delete the destination, drop the placement row
  move                       move the destination back to the source path

Safety checks run by default and refuse the revert if anything looks off:

  - Destination has drifted (someone edited the placed file): refuse.
  - Move-revert: source path already exists: refuse.

`--force` overrides both checks. Use sparingly — it can overwrite user data.

Destinations that are already missing on disk are warned and the placement \
row is dropped anyway (the file is gone either way). Source parent \
directories that don't exist for a move-revert are created automatically.

Refused when:
  - The target run doesn't exist.
  - The target was itself a dry-run (nothing to undo).
  - The target was itself a revert (no nested reverts).
  - The target was already reverted (use --force to re-revert).

EXAMPLES
    shelf revert 42                    # undo run 42 of the default profile
    shelf revert photos 42             # undo run 42 of `photos`
    shelf revert 42 --dry-run          # preview what would change
    shelf revert 42 --force            # override drift / source-exists checks
")]
    Revert(RevertArgs),
}
