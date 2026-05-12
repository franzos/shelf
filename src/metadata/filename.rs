//! Filename-based date extractor.
//!
//! Tries each pattern in `[metadata].filename_date_patterns` against the
//! file's stem in declared order. First successful `strptime` parse wins.
//! Patterns use chrono's strftime vocabulary.

use std::path::Path;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};

use super::Extractor;
use crate::error::Result;

pub struct FilenameExtractor {
    patterns: Vec<String>,
}

impl FilenameExtractor {
    #[must_use]
    pub fn new(patterns: Vec<String>) -> Self {
        Self { patterns }
    }
}

impl Extractor for FilenameExtractor {
    fn id(&self) -> &'static str {
        "filename"
    }

    fn try_date(&self, path: &Path, _kind: &str) -> Result<Option<NaiveDateTime>> {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            return Ok(None);
        };
        for pat in &self.patterns {
            if let Ok(dt) = NaiveDateTime::parse_from_str(stem, pat) {
                return Ok(Some(dt));
            }
            // Date-only patterns won't parse as a full datetime;
            // try as `NaiveDate` and synthesize midnight.
            if let Ok(d) = NaiveDate::parse_from_str(stem, pat) {
                return Ok(Some(NaiveDateTime::new(d, NaiveTime::MIN)));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn matches_known_pattern() {
        let ex = FilenameExtractor::new(vec!["IMG_%Y%m%d_%H%M%S".into()]);
        let p = PathBuf::from("/x/IMG_20240315_142210.jpg");
        let dt = ex.try_date(&p, "photo").unwrap().unwrap();
        assert_eq!(dt.to_string(), "2024-03-15 14:22:10");
    }

    #[test]
    fn falls_through_to_later_pattern() {
        let ex =
            FilenameExtractor::new(vec!["IMG_%Y%m%d_%H%M%S".into(), "VID_%Y%m%d_%H%M%S".into()]);
        let p = PathBuf::from("/x/VID_20240315_142210.mp4");
        let dt = ex.try_date(&p, "video").unwrap().unwrap();
        assert_eq!(dt.to_string(), "2024-03-15 14:22:10");
    }

    #[test]
    fn date_only_pattern_synthesizes_midnight() {
        let ex = FilenameExtractor::new(vec!["%Y-%m-%d".into()]);
        let p = PathBuf::from("/x/2024-03-15.pdf");
        let dt = ex.try_date(&p, "document").unwrap().unwrap();
        assert_eq!(dt.to_string(), "2024-03-15 00:00:00");
    }

    #[test]
    fn unmatched_returns_none() {
        let ex = FilenameExtractor::new(vec!["IMG_%Y%m%d_%H%M%S".into()]);
        let p = PathBuf::from("/x/random_name.jpg");
        assert!(ex.try_date(&p, "photo").unwrap().is_none());
    }

    #[test]
    fn empty_patterns_returns_none() {
        let ex = FilenameExtractor::new(vec![]);
        let p = PathBuf::from("/x/IMG_20240315_142210.jpg");
        assert!(ex.try_date(&p, "photo").unwrap().is_none());
    }
}
