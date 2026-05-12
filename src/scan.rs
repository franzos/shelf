//! Input walker + global include/exclude filtering.
//!
//! Glob matching operates on raw `OsStr` bytes via [`globset::Candidate`] so
//! non-UTF-8 filenames are matched on their underlying byte content rather
//! than a lossy decode. Globs are compiled case-insensitive (a `*.jpg`
//! include matches `DSC0001.JPG` straight off a camera).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use globset::{Candidate, GlobBuilder, GlobSet, GlobSetBuilder};
use walkdir::WalkDir;

use crate::config::{Filters, Profile};
use crate::error::{Error, Result};

/// A file the scanner accepted: located on disk, inside an input root, and
/// passing the profile's include/exclude filters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    pub source_root: PathBuf,
    pub absolute_path: PathBuf,
    /// Path relative to `source_root`, with `/` separators.
    pub relative_path: PathBuf,
}

/// Scan all inputs declared by `profile`.
pub fn scan_profile(profile: &Profile) -> Result<impl Iterator<Item = Result<ScannedFile>>> {
    scan(&profile.inputs, &profile.filters)
}

/// Walk `inputs` and yield files passing `filters`.
///
/// The returned iterator is `'static + Send`. Walking errors propagate as
/// `Err(Error::WalkDir { .. })` items; the iterator continues afterward.
pub fn scan(
    inputs: &[PathBuf],
    filters: &Filters,
) -> Result<impl Iterator<Item = Result<ScannedFile>> + Send + 'static> {
    let include = build_globset(&filters.include)?;
    let exclude = build_globset(&filters.exclude)?;
    let include = Arc::new(include);
    let exclude = Arc::new(exclude);

    let owned_inputs: Vec<PathBuf> = inputs.to_vec();

    let iter = owned_inputs.into_iter().flat_map(move |root| {
        let include = Arc::clone(&include);
        let exclude = Arc::clone(&exclude);
        let root_for_iter = root.clone();
        WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_map(move |entry| match entry {
                Ok(e) => {
                    if !e.file_type().is_file() {
                        return None;
                    }
                    let abs = e.path().to_path_buf();
                    let rel = match abs.strip_prefix(&root_for_iter) {
                        Ok(r) => r.to_path_buf(),
                        // Symlink resolution could in theory produce a path
                        // outside the root. Surfacing it as an error rather
                        // than matching against the abs path avoids a
                        // filter-bypass.
                        Err(_) => {
                            return Some(Err(Error::PathStripPrefix {
                                root: root_for_iter.clone(),
                                path: abs,
                            }));
                        }
                    };
                    if !accepts(&include, &exclude, &rel) {
                        return None;
                    }
                    Some(Ok(ScannedFile {
                        source_root: root_for_iter.clone(),
                        absolute_path: abs,
                        relative_path: rel,
                    }))
                }
                Err(err) => {
                    let path = err
                        .path()
                        .map_or_else(|| root_for_iter.clone(), Path::to_path_buf);
                    Some(Err(Error::WalkDir { path, source: err }))
                }
            })
    });

    Ok(iter)
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let glob = GlobBuilder::new(pat)
            .case_insensitive(true)
            .build()
            .map_err(|source| Error::BadGlob {
                pattern: pat.clone(),
                source,
            })?;
        builder.add(glob);
    }
    builder.build().map_err(|source| Error::BadGlob {
        pattern: patterns.join(", "),
        source,
    })
}

fn accepts(include: &GlobSet, exclude: &GlobSet, rel: &Path) -> bool {
    #[cfg(unix)]
    let candidate = Candidate::new(rel.as_os_str());

    #[cfg(not(unix))]
    let normalized: std::ffi::OsString = {
        let s = rel.to_string_lossy();
        if std::path::MAIN_SEPARATOR == '/' {
            s.into_owned().into()
        } else {
            s.replace(std::path::MAIN_SEPARATOR, "/").into()
        }
    };
    #[cfg(not(unix))]
    let candidate = Candidate::new(normalized.as_os_str());

    let included = include.is_empty() || include.is_match_candidate(&candidate);
    included && !exclude.is_match_candidate(&candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn empty_include_matches_everything() {
        let inc = build_globset(&[]).unwrap();
        let exc = build_globset(&[]).unwrap();
        assert!(accepts(&inc, &exc, &p("anything/at/all.jpg")));
    }

    #[test]
    fn include_filters_to_matches() {
        let inc = build_globset(&["*.jpg".to_string()]).unwrap();
        let exc = build_globset(&[]).unwrap();
        assert!(accepts(&inc, &exc, &p("a.jpg")));
        assert!(!accepts(&inc, &exc, &p("a.png")));
    }

    #[test]
    fn exclude_subtracts_from_include() {
        let inc = build_globset(&["**/*.jpg".to_string()]).unwrap();
        let exc = build_globset(&["**/cache/**".to_string()]).unwrap();
        assert!(accepts(&inc, &exc, &p("a/b.jpg")));
        assert!(!accepts(&inc, &exc, &p("a/cache/b.jpg")));
    }

    #[test]
    fn bad_glob_yields_error() {
        let err = build_globset(&["*[".to_string()]).unwrap_err();
        match err {
            Error::BadGlob { pattern, .. } => assert_eq!(pattern, "*["),
            other => panic!("expected BadGlob, got {other:?}"),
        }
    }

    #[test]
    fn include_matches_uppercase_extension() {
        let inc = build_globset(&["*.jpg".to_string()]).unwrap();
        let exc = build_globset(&[]).unwrap();
        assert!(accepts(&inc, &exc, &p("DSC0001.JPG")));
        assert!(accepts(&inc, &exc, &p("Mixed.Jpg")));
    }

    #[test]
    fn exclude_is_case_insensitive_symmetrically() {
        let inc = build_globset(&[]).unwrap();
        let exc = build_globset(&["*.GIF".to_string()]).unwrap();
        assert!(!accepts(&inc, &exc, &p("lower.gif")));
        assert!(!accepts(&inc, &exc, &p("UPPER.GIF")));
    }
}
