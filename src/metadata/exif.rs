//! EXIF extractor for JPEG / HEIC / TIFF.
//!
//! Reads `DateTimeOriginal` with `CreateDate` and `DateTime` as fallbacks.
//! The EXIF datetime format is `"YYYY:MM:DD HH:MM:SS"` — no timezone, by spec.
//!
//! "No EXIF" and "not a recognized container" map to `Ok(None)` so the
//! dispatch chain keeps walking. Only corrupted EXIF surfaces as
//! [`crate::error::Error::ExifParse`].

use std::cell::RefCell;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use chrono::NaiveDate;
use chrono::NaiveDateTime;
use exif::{In, Reader, Tag, Value};

use super::Extractor;
use crate::error::{Error, Result};

/// EXIF metadata extractor.
///
/// Single-slot path-keyed cache so multi-field extraction for one file
/// is a single open-and-parse.
#[derive(Default)]
pub struct ExifExtractor {
    cache: RefCell<Option<(PathBuf, Option<Rc<exif::Exif>>)>>,
}

impl ExifExtractor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read_exif_cached(&self, path: &Path) -> Result<Option<Rc<exif::Exif>>> {
        if let Some((cached_path, cached_exif)) = self.cache.borrow().as_ref()
            && cached_path == path
        {
            return Ok(cached_exif.clone());
        }
        let parsed = read_exif(path)?.map(Rc::new);
        *self.cache.borrow_mut() = Some((path.to_path_buf(), parsed.clone()));
        Ok(parsed)
    }
}

impl Extractor for ExifExtractor {
    fn id(&self) -> &'static str {
        "exif"
    }

    fn try_date(&self, path: &Path, _kind: &str) -> Result<Option<NaiveDateTime>> {
        let Some(exif) = self.read_exif_cached(path)? else {
            return Ok(None);
        };
        Ok(date_from(&exif))
    }

    fn try_camera(&self, path: &Path, _kind: &str) -> Result<Option<String>> {
        let Some(exif) = self.read_exif_cached(path)? else {
            return Ok(None);
        };
        Ok(camera_from(&exif))
    }

    fn try_lens(&self, path: &Path, _kind: &str) -> Result<Option<String>> {
        let Some(exif) = self.read_exif_cached(path)? else {
            return Ok(None);
        };
        Ok(lens_from(&exif))
    }

    fn try_dimensions(&self, path: &Path, _kind: &str) -> Result<Option<(u32, u32)>> {
        let Some(exif) = self.read_exif_cached(path)? else {
            return Ok(None);
        };
        Ok(dimensions_from(&exif))
    }
}

fn date_from(exif: &exif::Exif) -> Option<NaiveDateTime> {
    for tag in [Tag::DateTimeOriginal, Tag::DateTimeDigitized, Tag::DateTime] {
        if let Some(field) = exif.get_field(tag, In::PRIMARY)
            && let Some(dt) = field_to_naive_datetime(&field.value)
        {
            return Some(dt);
        }
    }
    None
}

fn camera_from(exif: &exif::Exif) -> Option<String> {
    let make = ascii_field(exif, Tag::Make);
    let model = ascii_field(exif, Tag::Model);
    combine_make_model(make, model)
}

fn lens_from(exif: &exif::Exif) -> Option<String> {
    ascii_field(exif, Tag::LensModel)
}

fn dimensions_from(exif: &exif::Exif) -> Option<(u32, u32)> {
    let w = exif
        .get_field(Tag::PixelXDimension, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0));
    let h = exif
        .get_field(Tag::PixelYDimension, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0));
    match (w, h) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    }
}

fn read_exif(path: &Path) -> Result<Option<exif::Exif>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(source) => {
            return Err(Error::MetadataIo {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let mut reader = BufReader::new(file);
    match Reader::new().read_from_container(&mut reader) {
        Ok(exif) => Ok(Some(exif)),
        // "no usable EXIF here" — dispatch chain handles it.
        Err(exif::Error::NotFound(_) | exif::Error::InvalidFormat(_)) => Ok(None),
        // BlankValue is hostile/malformed EXIF; treat as nothing-to-see
        // rather than aborting the whole extraction.
        Err(exif::Error::BlankValue(_)) => Ok(None),
        // Surface real I/O as MetadataIo so it isn't buried in ExifParse.
        Err(exif::Error::Io(source)) => Err(Error::MetadataIo {
            path: path.to_path_buf(),
            source,
        }),
        Err(source) => Err(Error::ExifParse {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn field_to_naive_datetime(value: &Value) -> Option<NaiveDateTime> {
    let Value::Ascii(ref vec) = *value else {
        return None;
    };
    let first = vec.first()?;
    let dt = exif::DateTime::from_ascii(first).ok()?;
    NaiveDate::from_ymd_opt(i32::from(dt.year), u32::from(dt.month), u32::from(dt.day))?
        .and_hms_opt(
            u32::from(dt.hour),
            u32::from(dt.minute),
            u32::from(dt.second),
        )
}

fn ascii_field(exif: &exif::Exif, tag: Tag) -> Option<String> {
    let field = exif.get_field(tag, In::PRIMARY)?;
    let Value::Ascii(ref vec) = field.value else {
        return None;
    };
    let bytes = vec.first()?;
    // EXIF ASCII is nominally 7-bit; tolerate stray UTF-8 from real cameras.
    let s = String::from_utf8_lossy(bytes).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Combine `Make` and `Model`, avoiding the common case where the model
/// already includes the maker (e.g. `Make = "NIKON CORPORATION"`,
/// `Model = "NIKON D850"` → `"NIKON D850"`).
fn combine_make_model(make: Option<String>, model: Option<String>) -> Option<String> {
    match (make, model) {
        (Some(mk), Some(md)) => {
            let mk_lc = mk.to_lowercase();
            let md_lc = md.to_lowercase();
            if md_lc.starts_with(&mk_lc) {
                return Some(md);
            }
            if let Some(first_word) = mk_lc.split_whitespace().next()
                && md_lc.starts_with(first_word)
            {
                return Some(md);
            }
            Some(format!("{mk} {md}"))
        }
        (Some(mk), None) => Some(mk),
        (None, Some(md)) => Some(md),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combines_make_and_model() {
        assert_eq!(
            combine_make_model(Some("Canon".into()), Some("EOS R5".into())),
            Some("Canon EOS R5".into())
        );
    }

    #[test]
    fn collapses_when_model_includes_make() {
        assert_eq!(
            combine_make_model(Some("NIKON CORPORATION".into()), Some("NIKON D850".into())),
            Some("NIKON D850".into())
        );
    }

    #[test]
    fn make_only() {
        assert_eq!(
            combine_make_model(Some("Sony".into()), None),
            Some("Sony".into())
        );
    }

    #[test]
    fn neither() {
        assert_eq!(combine_make_model(None, None), None);
    }
}
