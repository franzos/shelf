use std::path::PathBuf;
use thiserror::Error;

/// Per-field validation problem, collected by [`Error::Validation`].
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValidationError {
    #[error("`inputs` must contain at least one path")]
    NoInputs,
    #[error("`[[output]]` must be declared at least once")]
    NoOutputs,
    #[error("output names must be unique; `{0}` appears more than once")]
    DuplicateOutputName(String),
    #[error(
        "extension `{ext}` is claimed by multiple kinds (`{first}` and `{second}`); each canonical extension may only appear in one kind"
    )]
    ExtensionInMultipleKinds {
        ext: String,
        first: String,
        second: String,
    },
    #[error("invalid glob in {location}: `{pattern}` ({reason})")]
    BadGlob {
        location: String,
        pattern: String,
        reason: String,
    },
    #[error("invalid template in {location}: `{template}` ({reason})")]
    BadTemplate {
        location: String,
        template: String,
        reason: String,
    },
}

/// Crate-wide error type. Validation problems are collected into a single
/// [`Error::Validation`] so a broken profile reports every issue at once.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("io error at `{}`: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse TOML at `{}`: {source}", path.display())]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("profile `{name}` not found: no file at `{}`", path.display())]
    ProfileNotFound { name: String, path: PathBuf },

    #[error(
        "no profile name given and {count} profiles exist in `{}` ({}); pass a name or use --config",
        dir.display(),
        format_candidate_names(.names),
    )]
    ProfileAmbiguous {
        dir: PathBuf,
        count: usize,
        names: Vec<String>,
    },

    #[error("no profiles found in `{}`", dir.display())]
    NoProfiles { dir: PathBuf },

    #[error("could not determine config directory (set $SHELF_CONFIG_DIR or $XDG_CONFIG_HOME)")]
    NoConfigDir,

    #[error("profile `{}` failed validation:\n{}", path.display(), format_validation_errors(.errors))]
    Validation {
        path: PathBuf,
        errors: Vec<ValidationError>,
    },

    #[error("failed to walk `{}`: {source}", path.display())]
    WalkDir {
        path: PathBuf,
        #[source]
        source: walkdir::Error,
    },

    #[error("invalid glob `{pattern}`: {source}")]
    BadGlob {
        pattern: String,
        #[source]
        source: globset::Error,
    },

    #[error(
        "scanned path `{}` is not under input root `{}`",
        path.display(),
        root.display()
    )]
    PathStripPrefix { root: PathBuf, path: PathBuf },

    #[error(
        "no date extractor produced a value for `{}` (tried: {})",
        path.display(),
        sources_tried.join(", ")
    )]
    NoDate {
        path: PathBuf,
        sources_tried: Vec<String>,
    },

    #[error("failed to parse EXIF in `{}`: {source}", path.display())]
    ExifParse {
        path: PathBuf,
        #[source]
        source: exif::Error,
    },

    #[error("failed to parse MP4/MOV container in `{}`: {reason}", path.display())]
    Mp4Parse { path: PathBuf, reason: String },

    #[error("failed to parse PDF in `{}`: {reason}", path.display())]
    PdfParse { path: PathBuf, reason: String },

    #[error("metadata io error at `{}`: {source}", path.display())]
    MetadataIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to hash `{}`: {source}", path.display())]
    Hash {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("sqlite error at `{}`: {source}", path.display())]
    Sqlite {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("migration error on `{}`: {source}", path.display())]
    Migration {
        path: PathBuf,
        #[source]
        source: rusqlite_migration::Error,
    },

    #[error("could not determine state directory (set $XDG_DATA_HOME or $HOME)")]
    NoStateDir,

    #[error("apply failed ({kind}) at `{}`: {source}", path.display())]
    Apply {
        kind: ApplyErrorKind,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Template, validated up-front by [`crate::config::load_profile`], failed
    /// to (re)parse or render at planning time. Indicates a bug — validation
    /// should have caught it — but surfaces `location` and template body so
    /// the user has a thread to pull on.
    #[error("template error in {location}: `{template}` ({reason})")]
    Template {
        location: String,
        template: String,
        reason: String,
    },

    /// `shelf revert` refused to act. Distinct from `Unimplemented` so callers
    /// can recognise the case without string-matching.
    #[error("revert refused: {0}")]
    RevertRefused(String),

    #[error("not implemented in this milestone: `{0}`")]
    Unimplemented(&'static str),
}

/// Which filesystem step in [`crate::apply::apply`] surfaced an io error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApplyErrorKind {
    CreateDir,
    Tempfile,
    Copy,
    Fsync,
    Rename,
    Hardlink,
    Symlink,
    Remove,
}

impl std::fmt::Display for ApplyErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::CreateDir => "create-dir",
            Self::Tempfile => "tempfile",
            Self::Copy => "copy",
            Self::Fsync => "fsync",
            Self::Rename => "rename",
            Self::Hardlink => "hardlink",
            Self::Symlink => "symlink",
            Self::Remove => "remove",
        };
        f.write_str(s)
    }
}

fn format_validation_errors(errors: &[ValidationError]) -> String {
    let mut out = String::new();
    for (i, e) in errors.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str("  - ");
        out.push_str(&e.to_string());
    }
    out
}

fn format_candidate_names(names: &[String]) -> String {
    if names.is_empty() {
        return "no candidates".to_string();
    }
    let mut out = String::from("candidates: ");
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(n);
    }
    out
}

pub type Result<T> = std::result::Result<T, Error>;
