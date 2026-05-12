//! Kind classification.
//!
//! A `kind` is a profile-defined bucket (`photo`, `raw`, `video`, ...)
//! selected by a file's canonical extension. Anything not matched falls
//! into `"other"`. Validation guarantees each canonical extension appears
//! in at most one kind, so iteration order doesn't affect the result.

use std::collections::BTreeMap;

use crate::config::Profile;
use crate::extension::canonical_extension;
use crate::scan::ScannedFile;

pub const OTHER: &str = "other";

/// Resolve a canonical extension to its kind, or `"other"` if no kind claims
/// it (or the file had no extension).
#[must_use]
pub fn classify(canonical_ext: Option<&str>, kinds: &BTreeMap<String, Vec<String>>) -> String {
    let Some(ext) = canonical_ext else {
        return OTHER.to_string();
    };
    for (name, exts) in kinds {
        if exts.iter().any(|e| e == ext) {
            return name.clone();
        }
    }
    OTHER.to_string()
}

/// Canonical extension + resolved kind for a scanned file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    pub canonical_ext: Option<String>,
    pub kind: String,
}

/// Run extension normalization and kind classification for one scanned file.
#[must_use]
pub fn classify_scanned(profile: &Profile, file: &ScannedFile) -> Classified {
    let canonical_ext = canonical_extension(&file.absolute_path, &profile.extensions.canonical);
    let kind = classify(canonical_ext.as_deref(), &profile.kinds);
    Classified {
        canonical_ext,
        kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn photo_kinds() -> BTreeMap<String, Vec<String>> {
        let mut m = BTreeMap::new();
        m.insert(
            "photo".to_string(),
            vec!["jpg", "png", "heic", "webp", "tiff", "bmp"]
                .into_iter()
                .map(String::from)
                .collect(),
        );
        m.insert(
            "raw".to_string(),
            vec![
                "cr2", "cr3", "nef", "arw", "dng", "raf", "orf", "rw2", "srw",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        );
        m.insert(
            "video".to_string(),
            vec!["mp4", "mov", "mkv", "avi", "m4v", "mts", "m2ts"]
                .into_iter()
                .map(String::from)
                .collect(),
        );
        m
    }

    #[test]
    fn jpg_resolves_to_photo() {
        assert_eq!(classify(Some("jpg"), &photo_kinds()), "photo");
    }

    #[test]
    fn cr3_resolves_to_raw() {
        assert_eq!(classify(Some("cr3"), &photo_kinds()), "raw");
    }

    #[test]
    fn mp4_resolves_to_video() {
        assert_eq!(classify(Some("mp4"), &photo_kinds()), "video");
    }

    #[test]
    fn unknown_ext_is_other() {
        assert_eq!(classify(Some("xyz"), &photo_kinds()), "other");
    }

    #[test]
    fn none_is_other() {
        assert_eq!(classify(None, &photo_kinds()), "other");
    }

    #[test]
    fn empty_kinds_is_other() {
        assert_eq!(classify(Some("jpg"), &BTreeMap::new()), "other");
    }

    #[test]
    fn classify_scanned_uses_canonical_map_and_kinds() {
        let mut profile = Profile {
            inputs: vec![PathBuf::from("/in")],
            filters: Default::default(),
            extensions: Default::default(),
            kinds: photo_kinds(),
            metadata: Default::default(),
            sequence: Default::default(),
            dedupe: Default::default(),
            health: Default::default(),
            templates: Default::default(),
            state: Default::default(),
            outputs: Vec::new(),
        };
        profile
            .extensions
            .canonical
            .insert("jpeg".to_string(), "jpg".to_string());

        let file = ScannedFile {
            source_root: PathBuf::from("/in"),
            absolute_path: PathBuf::from("/in/holiday.JPEG"),
            relative_path: PathBuf::from("holiday.JPEG"),
        };

        let c = classify_scanned(&profile, &file);
        assert_eq!(c.canonical_ext.as_deref(), Some("jpg"));
        assert_eq!(c.kind, "photo");

        let oddball = ScannedFile {
            source_root: PathBuf::from("/in"),
            absolute_path: PathBuf::from("/in/notes.xyz"),
            relative_path: PathBuf::from("notes.xyz"),
        };
        let c2 = classify_scanned(&profile, &oddball);
        assert_eq!(c2.canonical_ext.as_deref(), Some("xyz"));
        assert_eq!(c2.kind, "other");

        let no_ext = ScannedFile {
            source_root: PathBuf::from("/in"),
            absolute_path: PathBuf::from("/in/README"),
            relative_path: PathBuf::from("README"),
        };
        let c3 = classify_scanned(&profile, &no_ext);
        assert_eq!(c3.canonical_ext, None);
        assert_eq!(c3.kind, "other");
    }
}
