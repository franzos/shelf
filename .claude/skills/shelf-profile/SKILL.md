---
name: shelf-profile
description: Author a new `shelf` profile (a TOML file in ~/.config/shelf/) by asking the user about their cataloguing workflow. Use when the user wants to set up `shelf` to organize a new kind of content — photos, videos, invoices, documents, downloads cleanup, or anything with file-level metadata.
---

You are authoring a profile for `shelf`, a CLI that catalogues files by metadata-driven rules. A profile is a single TOML file at `~/.config/shelf/<name>.toml` describing one workflow end-to-end: inputs, filters, kinds, metadata extractors, sequence, dedupe, and outputs.

## Arguments

$ARGUMENTS — if it names a workflow (e.g. "invoices", "photos"), use it as the profile-name suggestion and starting point. Otherwise, ask.

## Flow

1. **Discover the workflow.** Use `AskUserQuestion` to confirm:
   - Profile name (slug: `photos`, `invoices`, `downloads`, ...)
   - Input directories (one or more absolute paths)
   - Output directory (start with one)
   - Operation mode: `copy` (default, safe), `move` (destructive — confirm), `hardlink`, `symlink`
   - Kinds of files to organize — point them at the **Common kind classifications** below

2. **Discover the layout.** Ask the user to pick:
   - Directory pattern (`{yyyy}/{mm}` is the common photo choice; `{yyyy}/{mm}/{dd}` for fine-grained; `{kind}/{yyyy}/{mm}` to separate by type)
   - Filename pattern (`{yyyy}-{mm}-{dd}_{seq:05}` is the typical photo one; `{yyyy}-{mm}-{dd}_{author}_{seq:04}` for invoices)
   - Sequence scope: `day` (most common), `month`, `year`, `global`, or `folder`

   Don't ask about every config field — pick sensible defaults from the **Schema** below and only ask when the answer matters.

3. **Write the profile.** Use the `Write` tool to create `~/.config/shelf/<name>.toml`. Create the directory if missing (`mkdir -p` via Bash). Use the **Sample profiles** as a starting point and customize.

4. **Validate.** Run `shelf plan <name>` (it implies dry-run) and show the user the planned actions. If shelf is not on PATH, ask the user how it should be invoked (e.g. via `cargo run -- plan <name>` in the project, or a specific binary path). If validation surfaces errors, fix the profile and re-validate.

## Schema

```toml
inputs = ["/abs/path/one", "/abs/path/two"]      # required, at least one

[filters]                                         # global, applies before per-output kinds/match
include = ["*.jpg", "*.png"]                      # globs, empty = everything
exclude = ["**/cache/**"]                         # applied after include

[extensions.canonical]                            # normalization map
jpeg = "jpg"                                      # photos commonly use: jpeg→jpg, jpe→jpg,
heif = "heic"                                     #   tif→tiff, heif→heic, mpeg→mpg
tif  = "tiff"

[kinds]                                           # canonical exts grouped by kind name
photo = ["jpg", "png", "heic"]

[metadata]
date_sources = ["exif:DateTimeOriginal", "quicktime:CreationDate", "filename", "mtime"]
filename_date_patterns = ["IMG_%Y%m%d_%H%M%S"]    # strftime; tried in order

[sequence]
scope = "day"                                     # global | year | month | day | folder
start = 1

[dedupe]
strategy = "sha256"                               # sha256 | off
on_duplicate = "skip"                             # skip | replace | keep-both
scope = "output"                                  # output | global

[health]
flag_missing_date = true
flag_truncated = true

[templates.fallbacks]                             # per-token; default is "unknown"
camera = "unknown_camera"
author = "unknown_vendor"

[state]
database = "~/.local/share/shelf/<profile>.db"    # leave verbatim for default per-profile path

[[output]]                                        # one or more
name = "library"                                  # unique within profile
path = "/abs/destination"
mode = "copy"                                     # copy | move | hardlink | symlink
on_conflict = "rename"                            # skip | rename | replace | hash-suffix
directory = "{yyyy}/{mm}"                         # template, see tokens below
filename  = "{yyyy}-{mm}-{dd}_{seq:05}"           # template (no extension — appended)
preserve_mtime = true                             # default true; stamp dst with src's mtime
# optional per-output narrowing:
# kinds = ["photo", "video"]
# match = ["INVOICE_*.pdf"]
# optional per-kind template overrides:
# [output.directory_for]
# video = "{yyyy}/{mm}/videos"
# [output.filename_for]
# video = "{yyyy}-{mm}-{dd}_vid_{seq:05}"
```

### Template tokens

- **Date** (from `taken_at`): `{yyyy} {yy} {mm} {dd} {hh} {min} {ss}` — `{mm}` is month, `{min}` is minute
- **File**: `{ext}`, `{hash}` / `{hash:8}` (truncate to N hex), `{seq}` / `{seq:05}` (width)
- **Metadata**: `{camera} {lens} {kind} {author} {title} {vendor}`
- Modifiers: `:NN` (zero-pad width), `:raw` (skip slugification)
- Strings slugify by default: lowercase, spaces → `_`, strip anything outside `[a-z0-9_-]`
- Escape literal braces with `{{` and `}}`

### Common kind classifications

```toml
photo    = ["jpg", "png", "heic", "webp", "tiff", "bmp"]
raw      = ["cr2", "cr3", "nef", "arw", "dng", "raf", "orf", "rw2", "srw"]
video    = ["mp4", "mov", "mkv", "avi", "m4v", "mts", "m2ts"]
document = ["pdf", "epub", "docx", "odt"]
audio    = ["mp3", "flac", "wav", "ogg", "m4a"]
```

### `date_sources` by content

- Photos (mostly JPEG/HEIC): `["exif:DateTimeOriginal", "filename", "mtime"]`
- Mixed photo+video: `["exif:DateTimeOriginal", "quicktime:CreationDate", "filename", "mtime"]`
- Documents/invoices (PDF): `["pdf:CreationDate", "filename", "mtime"]`
- Downloads cleanup: `["exif:DateTimeOriginal", "quicktime:CreationDate", "pdf:CreationDate", "filename", "mtime"]`

## Sample profiles

### Photos (year/month, videos in a subfolder)

```toml
inputs = ["/home/user/dropbox/photos"]

[filters]
include = ["*.jpg", "*.jpeg", "*.png", "*.heic", "*.mp4", "*.mov", "*.cr3", "*.dng"]
exclude = ["**/cache/**", "**/.thumbnails/**"]

[extensions.canonical]
jpeg = "jpg"
heif = "heic"
tif  = "tiff"

[kinds]
photo = ["jpg", "png", "heic", "webp", "tiff"]
raw   = ["cr3", "nef", "arw", "dng"]
video = ["mp4", "mov", "m4v"]

[metadata]
date_sources = ["exif:DateTimeOriginal", "quicktime:CreationDate", "filename", "mtime"]

[sequence]
scope = "day"

[dedupe]
strategy = "sha256"
on_duplicate = "skip"
scope = "output"

[[output]]
name = "library"
path = "/home/user/library"
mode = "copy"
on_conflict = "rename"
directory = "{yyyy}/{mm}"
filename  = "{yyyy}-{mm}-{dd}_{seq:05}"

[output.directory_for]
video = "{yyyy}/{mm}/videos"
raw   = "{yyyy}/{mm}/raw"
```

### Invoices (PDFs, by year/month, author in filename)

```toml
inputs = ["/home/user/drop/invoices"]

[filters]
include = ["*.pdf"]

[kinds]
invoice = ["pdf"]

[metadata]
date_sources = ["pdf:CreationDate", "filename", "mtime"]
filename_date_patterns = ["%Y-%m-%d", "%Y%m%d"]

[templates.fallbacks]
author = "unknown_vendor"

[sequence]
scope = "month"

[dedupe]
strategy = "sha256"
on_duplicate = "skip"

[[output]]
name = "archive"
path = "/home/user/finance/invoices"
mode = "move"
on_conflict = "rename"
directory = "{yyyy}/{mm}"
filename  = "{yyyy}-{mm}-{dd}_{author}_{seq:04}"
```

### Downloads cleanup (mixed content, route by type)

```toml
inputs = ["/home/user/Downloads"]

[filters]
exclude = ["**/.cache/**", "*.crdownload", "*.part"]

[extensions.canonical]
jpeg = "jpg"

[kinds]
photo    = ["jpg", "png", "heic", "webp"]
video    = ["mp4", "mov", "mkv"]
document = ["pdf", "epub"]

[metadata]
date_sources = ["exif:DateTimeOriginal", "quicktime:CreationDate", "pdf:CreationDate", "filename", "mtime"]

[sequence]
scope = "month"

[dedupe]
strategy = "sha256"
on_duplicate = "skip"

[[output]]
name = "sorted"
path = "/home/user/Downloads/_sorted"
mode = "move"
on_conflict = "rename"
kinds = ["photo", "video", "document"]
directory = "{kind}/{yyyy}/{mm}"
filename  = "{yyyy}-{mm}-{dd}_{seq:04}"
```

## Things to watch

- **`mode = "move"`** is destructive. Default to `copy` unless the user has explicitly said "move". Re-confirm before writing the file.
- **Absolute paths** only in `inputs` and `output.path`. Relative paths are ambiguous and untested.
- **`exclude` patterns** match the path relative to each input root. `**/cache/**` excludes any `cache` folder anywhere; `cache/*` only excludes top-level.
- **One workflow per profile.** Don't try to combine "photos" and "invoices" in one — different extractors, different filename conventions. Make two files.
- **State DB is per-profile.** Renaming the profile filename creates a fresh DB; the old one still sits at `~/.local/share/shelf/<old-name>.db`. Mention this if the user is renaming.
- **`{seq}` resets per scope.** With `scope = "day"`, day boundaries reset the counter — re-running on the same day continues numbering, the next day starts at 1.
- **`preserve_mtime` defaults to `true`** — placed files keep the source's "last modified" timestamp. Set it to `false` on an output when you want destinations stamped with the wall-clock time of the run instead (rare; useful when an external tool indexes by "newest in this folder"). Same-fs `move` and `hardlink` keep mtime regardless; `symlink` targets the source which itself is unchanged.

## What NOT to do

- Don't invent template tokens. The list above is exhaustive — `{author}`/`{title}`/`{vendor}` only render values for PDF profiles where they were extracted; otherwise they fall through to the fallback map.
- Don't suggest features not in `shelf`: no watch/daemon mode, no perceptual dedupe, no editing/transcoding.
- Don't add `[state]` overrides unless the user has a specific reason (e.g. shared drive). The default per-profile XDG path is right for almost everyone.
- Don't validate via `shelf run` — that actually places files. Always validate via `shelf plan`.
