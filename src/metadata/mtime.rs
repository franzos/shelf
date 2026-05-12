//! Filesystem mtime fallback extractor.
//!
//! Reads `std::fs::metadata(path).modified()` and converts to local time,
//! discarding the timezone offset to land in [`NaiveDateTime`]. The local
//! interpretation matches EXIF's spec so a chain of `[exif, filename, mtime]`
//! produces comparable values.

use std::path::Path;
use std::time::UNIX_EPOCH;

use chrono::{DateTime, Local, NaiveDateTime};

use super::Extractor;
use crate::error::{Error, Result};

pub struct MtimeExtractor;

impl Extractor for MtimeExtractor {
    fn id(&self) -> &'static str {
        "mtime"
    }

    fn try_date(&self, path: &Path, _kind: &str) -> Result<Option<NaiveDateTime>> {
        let meta = std::fs::metadata(path).map_err(|source| Error::MetadataIo {
            path: path.to_path_buf(),
            source,
        })?;
        let modified = meta.modified().map_err(|source| Error::MetadataIo {
            path: path.to_path_buf(),
            source,
        })?;

        // Avoid `DateTime::<Local>::from(SystemTime)`, which panics on
        // out-of-range values. Convert via UNIX_EPOCH so junk timestamps
        // become `Ok(None)` instead.
        let Ok(since_epoch) = modified.duration_since(UNIX_EPOCH) else {
            return Ok(None);
        };
        let secs = i64::try_from(since_epoch.as_secs()).ok();
        let nanos = since_epoch.subsec_nanos();
        let Some(secs) = secs else {
            return Ok(None);
        };
        let Some(dt) = DateTime::from_timestamp(secs, nanos) else {
            return Ok(None);
        };
        Ok(Some(dt.with_timezone(&Local).naive_local()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{FileTime, set_file_mtime};
    use tempfile::NamedTempFile;

    #[test]
    fn reads_mtime_into_naive_datetime() {
        let tmp = NamedTempFile::new().unwrap();
        let ts_unix: i64 = 1_710_512_530;
        set_file_mtime(tmp.path(), FileTime::from_unix_time(ts_unix, 0)).unwrap();

        let dt = MtimeExtractor
            .try_date(tmp.path(), "photo")
            .unwrap()
            .unwrap();

        let expected = chrono::DateTime::from_timestamp(ts_unix, 0)
            .unwrap()
            .with_timezone(&Local)
            .naive_local();
        assert_eq!(dt, expected);
    }
}
