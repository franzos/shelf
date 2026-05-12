//! Typed profile schema.
//!
//! Parsing here is a pure serde concern. Semantic checks live in
//! [`super::validate`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Profile {
    /// Source roots. Required, non-empty (enforced by validation).
    pub inputs: Vec<PathBuf>,

    #[serde(default)]
    pub filters: Filters,

    #[serde(default)]
    pub extensions: Extensions,

    /// Map of kind-name (e.g. `"photo"`) to its canonical extensions.
    #[serde(default)]
    pub kinds: BTreeMap<String, Vec<String>>,

    #[serde(default)]
    pub metadata: Metadata,

    #[serde(default)]
    pub sequence: Sequence,

    #[serde(default)]
    pub dedupe: Dedupe,

    #[serde(default)]
    pub health: Health,

    #[serde(default)]
    pub templates: TemplatesConfig,

    #[serde(default)]
    pub state: StateCfg,

    /// Output destinations. TOML key `[[output]]`; required, non-empty.
    #[serde(rename = "output", default)]
    pub outputs: Vec<Output>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Filters {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Extensions {
    #[serde(default)]
    pub canonical: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Metadata {
    #[serde(default)]
    pub date_sources: Vec<String>,
    #[serde(default)]
    pub filename_date_patterns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Sequence {
    #[serde(default)]
    pub scope: SequenceScope,
    #[serde(default = "Sequence::default_start")]
    pub start: u64,
}

impl Sequence {
    fn default_start() -> u64 {
        1
    }
}

impl Default for Sequence {
    fn default() -> Self {
        Self {
            scope: SequenceScope::default(),
            start: Self::default_start(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SequenceScope {
    Global,
    Year,
    Month,
    #[default]
    Day,
    Folder,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Dedupe {
    #[serde(default)]
    pub strategy: DedupeStrategy,
    #[serde(default)]
    pub on_duplicate: OnDuplicate,
    #[serde(default)]
    pub scope: DedupeScope,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DedupeStrategy {
    #[default]
    Sha256,
    Off,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnDuplicate {
    #[default]
    Skip,
    Replace,
    KeepBoth,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DedupeScope {
    #[default]
    Output,
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Health {
    #[serde(default = "default_true")]
    pub flag_missing_date: bool,
    #[serde(default = "default_true")]
    pub flag_truncated: bool,
    #[serde(default)]
    pub verify_on_rerun: bool,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            flag_missing_date: true,
            flag_truncated: true,
            verify_on_rerun: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TemplatesConfig {
    #[serde(default)]
    pub fallbacks: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StateCfg {
    #[serde(default = "StateCfg::default_db_path")]
    pub database: PathBuf,
}

impl StateCfg {
    fn default_db_path() -> PathBuf {
        // Resolved against the real XDG state dir at runtime.
        PathBuf::from("~/.local/share/shelf/<profile>.db")
    }
}

impl Default for StateCfg {
    fn default() -> Self {
        Self {
            database: Self::default_db_path(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Output {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub mode: OpMode,
    #[serde(default)]
    pub on_conflict: OnConflict,

    pub directory: String,
    pub filename: String,

    /// Per-output kind filter. `None` accepts any kind.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,

    /// Per-output filename-glob filter. `None` accepts any name.
    #[serde(default, rename = "match")]
    pub match_: Option<Vec<String>>,

    /// `kind -> directory template` overrides.
    #[serde(default)]
    pub directory_for: BTreeMap<String, String>,

    /// `kind -> filename template` overrides.
    #[serde(default)]
    pub filename_for: BTreeMap<String, String>,

    /// Carry source mtime to the destination. Defaults to `true`,
    /// matching rsync. Applies to `copy` and `move` (cross-device
    /// fallback); `move` on the same FS, `hardlink`, and `symlink`
    /// preserve mtime by definition.
    #[serde(default = "default_true")]
    pub preserve_mtime: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpMode {
    #[default]
    Copy,
    Move,
    Hardlink,
    Symlink,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnConflict {
    #[default]
    Skip,
    Rename,
    Replace,
    HashSuffix,
}

/// Read a profile from disk and run validation.
pub fn load_profile(path: &Path) -> Result<Profile> {
    let raw = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let profile: Profile = toml::from_str(&raw).map_err(|source| Error::Toml {
        path: path.to_path_buf(),
        source,
    })?;
    let errors = super::validate::validate(&profile);
    if errors.is_empty() {
        Ok(profile)
    } else {
        Err(Error::Validation {
            path: path.to_path_buf(),
            errors,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHOTOS_TOML: &str = r#"
inputs = [
  "/home/user/sources/phone",
  "/home/user/sources/camera",
]

[filters]
include = ["*.jpg", "*.png", "*.mp4", "*.heic", "*.mov", "*.cr3"]
exclude = ["*.gif", "**/cache/**", "**/.thumbnails/**"]

[extensions.canonical]
jpeg = "jpg"
jpe  = "jpg"
tif  = "tiff"
heif = "heic"
mpeg = "mpg"

[kinds]
photo = ["jpg", "png", "heic", "webp", "tiff", "bmp"]
raw   = ["cr2", "cr3", "nef", "arw", "dng", "raf", "orf", "rw2", "srw"]
video = ["mp4", "mov", "mkv", "avi", "m4v", "mts", "m2ts"]

[metadata]
date_sources = ["exif:DateTimeOriginal", "quicktime:CreationDate", "filename", "mtime"]
filename_date_patterns = ["IMG_%Y%m%d_%H%M%S", "VID_%Y%m%d_%H%M%S"]

[sequence]
scope = "day"
start = 1

[dedupe]
strategy = "sha256"
on_duplicate = "skip"
scope = "output"

[health]
flag_missing_date = true
flag_truncated = true
verify_on_rerun = false

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
"#;

    const INVOICES_TOML: &str = r#"
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
"#;

    fn parse_str(s: &str) -> Profile {
        toml::from_str(s).expect("parse")
    }

    #[test]
    fn parses_photos_profile() {
        let p = parse_str(PHOTOS_TOML);
        assert_eq!(p.inputs.len(), 2);
        assert_eq!(p.filters.include.len(), 6);
        assert_eq!(p.filters.exclude.len(), 3);
        assert_eq!(p.extensions.canonical.get("jpeg").unwrap(), "jpg");
        assert_eq!(p.kinds["photo"].len(), 6);
        assert_eq!(p.kinds["raw"].len(), 9);
        assert_eq!(p.metadata.date_sources.len(), 4);
        assert_eq!(p.sequence.scope, SequenceScope::Day);
        assert_eq!(p.sequence.start, 1);
        assert_eq!(p.dedupe.strategy, DedupeStrategy::Sha256);
        assert_eq!(p.dedupe.on_duplicate, OnDuplicate::Skip);
        assert_eq!(p.dedupe.scope, DedupeScope::Output);
        assert!(p.health.flag_missing_date);
        assert!(p.health.flag_truncated);
        assert!(!p.health.verify_on_rerun);
        assert_eq!(p.outputs.len(), 1);
        let o = &p.outputs[0];
        assert_eq!(o.name, "library");
        assert_eq!(o.mode, OpMode::Copy);
        assert_eq!(o.on_conflict, OnConflict::Rename);
        assert_eq!(o.directory, "{yyyy}/{mm}");
        assert_eq!(o.directory_for["video"], "{yyyy}/{mm}/videos");
        assert_eq!(o.directory_for["raw"], "{yyyy}/{mm}/raw");
        assert!(o.filename_for.is_empty());
        assert!(o.kinds.is_none());
        assert!(o.match_.is_none());
        assert!(o.preserve_mtime, "preserve_mtime defaults to true");
    }

    #[test]
    fn parses_invoices_profile() {
        let p = parse_str(INVOICES_TOML);
        assert_eq!(p.inputs.len(), 1);
        assert_eq!(p.kinds["invoice"], vec!["pdf"]);
        assert_eq!(p.sequence.scope, SequenceScope::Month);
        assert_eq!(p.sequence.start, 1);
        assert_eq!(
            p.templates.fallbacks.get("author").unwrap(),
            "unknown_vendor"
        );
        assert_eq!(p.outputs[0].mode, OpMode::Move);
        assert_eq!(p.outputs[0].filename, "{yyyy}-{mm}-{dd}_{author}_{seq:04}");
    }

    #[test]
    fn preserve_mtime_can_be_disabled_per_output() {
        let toml_str = r#"
inputs = ["/tmp/i"]

[[output]]
name = "lib"
path = "/tmp/o"
directory = "{yyyy}"
filename = "{yyyy}-{mm}"
preserve_mtime = false
"#;
        let p: Profile = toml::from_str(toml_str).unwrap();
        assert!(!p.outputs[0].preserve_mtime);
    }

    #[test]
    fn photos_profile_validates_clean() {
        let p = parse_str(PHOTOS_TOML);
        let errs = crate::config::validate::validate(&p);
        assert!(errs.is_empty(), "expected clean validation, got {errs:?}");
    }

    #[test]
    fn invoices_profile_validates_clean() {
        let p = parse_str(INVOICES_TOML);
        let errs = crate::config::validate::validate(&p);
        assert!(errs.is_empty(), "expected clean validation, got {errs:?}");
    }

    #[test]
    fn missing_inputs_field_fails() {
        // `inputs` is required (no `#[serde(default)]`).
        let toml_str = r#"
[[output]]
name = "x"
path = "/tmp/x"
directory = "{yyyy}"
filename = "{yyyy}-{mm}"
"#;
        let err = toml::from_str::<Profile>(toml_str).unwrap_err();
        assert!(
            err.to_string().contains("inputs"),
            "expected error to mention `inputs`, got: {err}"
        );
    }

    #[test]
    fn unknown_enum_value_fails() {
        let toml_str = r#"
inputs = ["/tmp/i"]

[sequence]
scope = "fortnight"

[[output]]
name = "x"
path = "/tmp/x"
directory = "{yyyy}"
filename = "{yyyy}"
"#;
        let err = toml::from_str::<Profile>(toml_str).unwrap_err();
        let msg = err.to_string();
        // `scope` pins this to the right field; without it the test could
        // pass on any unrelated parse error containing "variant"/"fortnight".
        assert!(
            msg.contains("scope") && (msg.contains("fortnight") || msg.contains("variant")),
            "expected enum error about `scope`, got: {msg}"
        );
    }

    #[test]
    fn no_outputs_is_validation_error() {
        let toml_str = r#"
inputs = ["/tmp/i"]
"#;
        let p: Profile = toml::from_str(toml_str).unwrap();
        let errs = crate::config::validate::validate(&p);
        assert!(errs.contains(&crate::error::ValidationError::NoOutputs));
    }

    #[test]
    fn duplicate_output_name_is_validation_error() {
        let toml_str = r#"
inputs = ["/tmp/i"]

[[output]]
name = "dup"
path = "/tmp/a"
directory = "{yyyy}"
filename = "{yyyy}-{mm}"

[[output]]
name = "dup"
path = "/tmp/b"
directory = "{yyyy}"
filename = "{yyyy}-{mm}"
"#;
        let p: Profile = toml::from_str(toml_str).unwrap();
        let errs = crate::config::validate::validate(&p);
        assert!(errs.iter().any(|e| matches!(
            e,
            crate::error::ValidationError::DuplicateOutputName(n) if n == "dup"
        )));
    }

    #[test]
    fn extension_in_two_kinds_is_validation_error() {
        let toml_str = r#"
inputs = ["/tmp/i"]

[kinds]
photo = ["jpg", "png"]
raw   = ["jpg", "cr3"]

[[output]]
name = "lib"
path = "/tmp/o"
directory = "{yyyy}"
filename = "{yyyy}-{mm}"
"#;
        let p: Profile = toml::from_str(toml_str).unwrap();
        let errs = crate::config::validate::validate(&p);
        assert!(errs.iter().any(|e| matches!(
            e,
            crate::error::ValidationError::ExtensionInMultipleKinds { ext, .. } if ext == "jpg"
        )));
    }

    #[test]
    fn bad_glob_is_validation_error() {
        let toml_str = r#"
inputs = ["/tmp/i"]

[filters]
include = ["*["]

[[output]]
name = "lib"
path = "/tmp/o"
directory = "{yyyy}"
filename = "{yyyy}-{mm}"
"#;
        let p: Profile = toml::from_str(toml_str).unwrap();
        let errs = crate::config::validate::validate(&p);
        assert!(
            errs.iter()
                .any(|e| matches!(e, crate::error::ValidationError::BadGlob { .. })),
            "expected BadGlob, got {errs:?}"
        );
    }
}
