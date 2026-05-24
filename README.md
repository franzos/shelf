# shelf

<p align="center">
  <img src="assets/logo.svg" alt="shelf" width="480">
</p>
<p align="center">
  A CLI for cataloguing files by metadata-driven rules. Walks input folders,
  extracts metadata, sorts files into a structured destination via templates,
  deduplicates by content hash, and tracks state so re-runs are cheap and
  deterministic.
</p>

Built first for photos and videos, but the pipeline is generic — profiles
target documents, invoices, downloads, or anything with file-level metadata.

## Features

- **Profile-driven**. Each workflow is a single TOML file in `~/.config/shelf/`.
- **Metadata-first sorting**. EXIF for photos, QuickTime/MP4 for videos,
  PDF `/Info` for documents. Falls back through filename patterns to `mtime`.
- **Templates for paths and filenames**. Curly-brace tokens like
  `{yyyy}/{mm}/{dd}_{seq:05}` and `{camera}` with `:raw` and width modifiers.
- **Per-day stable sequence numbers** that survive reruns.
- **Content-based dedupe** via sha256. Resized or recompressed copies are
  treated as distinct files.
- **Atomic file ops**. Temp + fsync + rename — a crash never leaves a
  half-written file at the destination.
- **Health checks**: truncated files, missing capture date, hash drift,
  orphan files, unrouted files.
- **Run history & revert**. Every `shelf run` is logged; `shelf revert <id>`
  undoes a prior run, op-mode-aware.
- **Ad-hoc imports** via `--from /path` — point shelf at any directory and
  use a profile's rules for one run.
- **Re-runnable**. SQLite state DB tracks what's been placed; subsequent runs
  only act on new files.

## Non-goals

- No daemon or watch mode — runs are explicit and one-shot. Schedule with
  cron or a systemd timer.
- No perceptual dedupe. Two visually similar files with different bytes are
  different files.
- No editing, transcoding, or thumbnail generation.

## Install

| Method | Command |
|--------|---------|
| Homebrew | `brew tap franzos/tap && brew install shelf` |
| Debian/Ubuntu | Download [`.deb`](https://github.com/franzos/shelf/releases) — `sudo dpkg -i shelf_*_amd64.deb` |
| Fedora/RHEL | Download [`.rpm`](https://github.com/franzos/shelf/releases) — `sudo rpm -i shelf-*.x86_64.rpm` |
| Guix | `guix shell -m manifest.scm -- cargo build --release` |
| Cargo | `cargo build --release` |

Pre-built binaries for Linux (x86_64), macOS (Apple Silicon, Intel) on [GitHub Releases](https://github.com/franzos/shelf/releases).

## Quickstart

1. **Write a profile.** Easiest way: use the bundled Claude Code skill —
   `/shelf-profile` walks you through it. Or copy a sample from
   `.claude/skills/shelf-profile/SKILL.md` and edit.

2. **See what shelf would do** without touching anything:

   ```bash
   shelf plan photos
   ```

3. **Run it for real**:

   ```bash
   shelf run photos
   ```

4. **Schedule reruns** via cron — shelf will only act on files added since
   the last run.

## Subcommands

```
shelf run    [profile] [--from PATH]... [--dry-run] [--strict] [--all]
shelf plan   [profile] [--from PATH]... [--all]        # alias for run --dry-run
shelf health [profile] [--sample N]                    # diagnostic report
shelf verify [profile] [--full | --sample N]           # rehash placements, flag drift
shelf runs   [profile] [<id>] [--limit N]              # run history; <id> shows placements
shelf revert [profile] <id> [--dry-run] [--force]      # undo a prior run
shelf status                                           # all profiles, counts, last run
shelf list                                             # profiles in the config dir
```

Global flags: `--config PATH`, `-v` / `-vv` (verbosity). Every subcommand has
a `--help` with full details and examples.

## Ad-hoc import

Override a profile's inputs for one run — useful for SD cards, friend's
photos, or any directory you want to import once:

```bash
shelf run photos --from /mnt/sdcard
shelf plan photos --from /a/path --from /another  # repeatable
```

Filters, dedupe, state DB, and sequence numbering all apply normally. Only
the scan roots change.

## Run history & revert

Every `shelf run` writes a row to the `runs` table. `shelf runs` lists them
newest-first; `shelf runs <id>` shows the placements that run produced.

Each row carries a status:

- `(none)` — finished cleanly.
- `(dry-run)` — `--dry-run`; nothing was placed.
- `(incomplete)` — the process died mid-run; placements may exist on disk
  without a finished row. Treat as "run again or revert manually".
- `reverted by <id>` — already undone by a later revert.

`shelf revert <id>` undoes a run. The op mode is remembered per placement:
copy/hardlink/symlink reverts delete the destination; move reverts put the
file back at its original source path.

```bash
shelf revert 42                    # undo run 42 (default profile)
shelf revert photos 42             # undo run 42 of profile `photos`
shelf revert 42 --dry-run          # preview
shelf revert 42 --force            # override safety checks
```

Safety checks refuse without `--force` when the destination has drifted
(someone edited the placed file) or when a move-revert would clobber an
existing source path. A few refusals are unconditional — `--force` won't
bypass them: the target run doesn't exist, was itself a dry-run, or was
itself a revert.

## Exit codes

| Code | Meaning                                            |
| ---- | -------------------------------------------------- |
| 0    | Success                                            |
| 1    | Runtime error (I/O, SQLite, walk, hash, ...)       |
| 2    | Structural error (profile not found, validation)   |
| 3    | `--strict` promoted health entries to failure      |
| 4    | `health` / `verify` found issues                   |

## Where things live

- **Profiles**: `~/.config/shelf/<name>.toml` (override with
  `$SHELF_CONFIG_DIR`, `$XDG_CONFIG_HOME`, or `--config PATH`).
- **State**: `$XDG_DATA_HOME/shelf/<profile>.db`, falling back to
  `~/.local/share/shelf/<profile>.db`.
- **Profile schema reference**: `.claude/skills/shelf-profile/SKILL.md`.

## Profile reference

The skill at `.claude/skills/shelf-profile/SKILL.md` has the full schema
plus three sample profiles (photos, invoices, downloads). Short version:

```toml
inputs = ["/abs/path/to/source"]

[filters]
include = ["*.jpg", "*.png", "*.mp4"]
exclude = ["**/cache/**"]

[kinds]
photo = ["jpg", "png", "heic"]
video = ["mp4", "mov"]

[metadata]
date_sources = ["exif:DateTimeOriginal", "quicktime:CreationDate", "filename", "mtime"]

[sequence]
scope = "day"

[dedupe]
strategy = "sha256"
on_duplicate = "skip"

[[output]]
name = "library"
path = "/abs/path/to/destination"
mode = "copy"             # copy | move | hardlink | symlink
on_conflict = "rename"    # skip | rename | replace | hash-suffix
preserve_mtime = true     # default
directory = "{yyyy}/{mm}"
filename  = "{yyyy}-{mm}-{dd}_{seq:05}"
```

## A note on safety

`mode = "move"` is destructive. Default to `copy` and confirm a few dry-runs
before switching. The state DB lets shelf detect already-placed files on
subsequent runs, so a `copy`-then-`move` migration is fine — you won't end up
with duplicates.

If a run goes sideways, `shelf revert <id>` will put copies back (delete
dests) or moves back (restore sources). It's not a substitute for backups,
but it's a fast first response.

## FAQ

The mental model: **source folders are your ingestion queue**, **the
destination is your library**. Once shelf has placed a file, it's yours to
cull, edit, rename, or move — shelf gets out of the way. The state DB tracks
"I've handled this source file," not "this destination must exist."

**What if I edit a destination file (Photoshop, sidecar, save-over)?**
shelf leaves it alone. The source is unchanged, so dedupe skips on rerun and
your edit survives. `shelf verify --full` notes the byte mismatch as
`health: drift`; advisory, no action.

**What if I delete a destination file?**
The placement row still says "this sha256 is placed here," so dedupe treats
the source as handled and **does not** re-place it. `shelf health` surfaces
the absence as `missing-destination`. To get it back, drop the placement row
and rerun:

```bash
sqlite3 ~/.local/share/shelf/photos.db \
  "DELETE FROM placements WHERE dest_path = '/path/to/file.jpg';"
shelf run photos
```

**What if I rename or move a destination file (e.g. organizing into albums)?**
The original path shows up as `missing-destination`, the new path as `orphan`.
shelf doesn't know they're the same file, but it also doesn't undo your
reorganization.

**What if I delete a source file?**
Destination is untouched. `shelf verify` flags it as `missing-source`.
Rerunning just won't see that source again.

**Why doesn't shelf re-place files I deleted from the destination?**
On purpose. A tool that "noticed" your culls and re-added them would be
infuriating for a photo library. To force re-handling, drop the placement
row (above).

**How do I start over with a profile?**
Delete the state DB and the destination tree, then rerun:

```bash
rm ~/.local/share/shelf/photos.db
rm -rf /path/to/destination          # or just empty the relevant subtrees
shelf run photos
```

**Why does `shelf health` keep reporting old drift entries?**
The `health` table accumulates entries; there's no auto-cleanup yet. They're
advisory. Clear them with:

```bash
sqlite3 ~/.local/share/shelf/photos.db "DELETE FROM health;"
```

**I just ran something I didn't mean to — can I undo it?**
Yes:

```bash
shelf runs photos                # find the run id
shelf revert photos 42 --dry-run # see what would happen
shelf revert photos 42           # actually undo it
```
