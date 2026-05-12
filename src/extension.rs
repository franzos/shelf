//! Canonical extension normalization.

use std::collections::BTreeMap;
use std::path::Path;

/// Return the canonical, lowercased extension for `path`.
///
/// Returns `None` when the path has no extension or the extension contains
/// non-UTF-8 bytes. When the lowercased extension is not in `canonical_map`,
/// it is returned as-is — making the function idempotent on canonical input.
#[must_use]
pub fn canonical_extension(
    path: &Path,
    canonical_map: &BTreeMap<String, String>,
) -> Option<String> {
    let ext = path.extension()?.to_str()?;
    let lower = ext.to_ascii_lowercase();
    Some(canonical_map.get(&lower).cloned().unwrap_or(lower))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn photo_map() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("jpeg".to_string(), "jpg".to_string());
        m.insert("jpe".to_string(), "jpg".to_string());
        m.insert("tif".to_string(), "tiff".to_string());
        m.insert("heif".to_string(), "heic".to_string());
        m
    }

    #[test]
    fn jpeg_variants_collapse_to_jpg() {
        let m = photo_map();
        assert_eq!(
            canonical_extension(&PathBuf::from("a.Jpg"), &m).as_deref(),
            Some("jpg"),
        );
        assert_eq!(
            canonical_extension(&PathBuf::from("a.JPEG"), &m).as_deref(),
            Some("jpg"),
        );
        assert_eq!(
            canonical_extension(&PathBuf::from("a.jpe"), &m).as_deref(),
            Some("jpg"),
        );
        assert_eq!(
            canonical_extension(&PathBuf::from("a.JPG"), &m).as_deref(),
            Some("jpg"),
        );
    }

    #[test]
    fn tif_collapses_to_tiff() {
        let m = photo_map();
        assert_eq!(
            canonical_extension(&PathBuf::from("scan.tif"), &m).as_deref(),
            Some("tiff"),
        );
    }

    #[test]
    fn no_extension_yields_none() {
        let m = photo_map();
        assert_eq!(canonical_extension(&PathBuf::from("README"), &m), None);
        assert_eq!(canonical_extension(&PathBuf::from(""), &m), None);
    }

    #[test]
    fn already_canonical_is_unchanged() {
        let m = photo_map();
        assert_eq!(
            canonical_extension(&PathBuf::from("a.png"), &m).as_deref(),
            Some("png"),
        );
        assert_eq!(
            canonical_extension(&PathBuf::from("a.cr3"), &m).as_deref(),
            Some("cr3"),
        );
    }

    #[test]
    fn empty_map_just_lowercases() {
        let m = BTreeMap::new();
        assert_eq!(
            canonical_extension(&PathBuf::from("a.JPG"), &m).as_deref(),
            Some("jpg"),
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 32,
            ..proptest::test_runner::Config::default()
        })]

        /// Idempotence holds when the map has no cycles; we filter cycle-y
        /// entries to match the invariant a well-formed profile satisfies.
        #[test]
        fn canonical_map_is_idempotent(
            ext in "[a-z0-9]{1,6}",
            entries in proptest::collection::vec(
                ("[a-z0-9]{1,6}", "[a-z0-9]{1,6}"),
                0..8,
            ),
        ) {
            let mut map: BTreeMap<String, String> = entries.into_iter().collect();
            let keys: std::collections::BTreeSet<String> = map.keys().cloned().collect();
            map.retain(|_, v| !keys.contains(v));

            let path = PathBuf::from(format!("file.{ext}"));
            let once = canonical_extension(&path, &map);
            let twice = once
                .as_ref()
                .and_then(|e| canonical_extension(&PathBuf::from(format!("file.{e}")), &map));
            proptest::prop_assert_eq!(once, twice);
        }
    }
}
