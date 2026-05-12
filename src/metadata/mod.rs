//! Metadata extraction.
//!
//! Each file goes through a chain of [`Extractor`]s declared in
//! `[metadata].date_sources`. First to produce a `taken_at` wins; the
//! source is recorded on [`Metadata`] so health checks can flag files
//! that fell through to `mtime`.

pub mod exif;
pub mod filename;
pub mod mtime;
pub mod pdf;
pub mod quicktime;

#[cfg(test)]
mod quicktime_test_fixtures;

use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;

use crate::config::Profile;
use crate::error::{Error, Result};
use crate::scan::ScannedFile;

/// Normalized metadata record for a single file. Only `taken_at` is required.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Metadata {
    /// Capture (or document) date. Local naive: no timezone is attached.
    /// EXIF `DateTimeOriginal` is by spec local time without zone, and
    /// mtime is interpreted as local time too.
    pub taken_at: NaiveDateTime,
    pub taken_at_source: DateSource,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub kind: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub author: Option<String>,
    pub title: Option<String>,
    /// Synonym for `author` in the invoices template. Populated from
    /// `/Info /Author`, falling back to `/Producer`.
    pub vendor: Option<String>,
}

impl Metadata {
    #[must_use]
    pub fn minimal(taken_at: NaiveDateTime, taken_at_source: DateSource, kind: String) -> Self {
        Self {
            taken_at,
            taken_at_source,
            camera: None,
            lens: None,
            kind,
            width: None,
            height: None,
            author: None,
            title: None,
            vendor: None,
        }
    }
}

/// Which extractor produced the `taken_at` date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DateSource {
    Exif,
    Quicktime,
    Pdf,
    Filename,
    Mtime,
}

/// A pluggable metadata extractor.
///
/// Implementations return `Ok(None)` when the file simply doesn't carry
/// the requested field; only true parse failures should surface as `Err`.
/// The dispatcher treats `Ok(None)` as "try the next source".
pub trait Extractor {
    fn id(&self) -> &'static str;

    fn try_date(&self, path: &Path, kind: &str) -> Result<Option<NaiveDateTime>>;

    fn try_camera(&self, _path: &Path, _kind: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn try_lens(&self, _path: &Path, _kind: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn try_dimensions(&self, _path: &Path, _kind: &str) -> Result<Option<(u32, u32)>> {
        Ok(None)
    }

    fn try_author(&self, _path: &Path, _kind: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn try_title(&self, _path: &Path, _kind: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn try_vendor(&self, _path: &Path, _kind: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

/// Parsed shape of a `[metadata].date_sources` entry.
///
/// Entries are bare (`"filename"`) or namespaced (`"exif:DateTimeOriginal"`).
/// The subkey is accepted for forward compatibility but currently ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceSpec {
    Exif,
    Quicktime,
    Pdf,
    Filename,
    Mtime,
}

impl SourceSpec {
    fn parse(raw: &str) -> Option<Self> {
        let (head, _tail) = raw.split_once(':').unwrap_or((raw, ""));
        match head {
            "exif" => Some(Self::Exif),
            "quicktime" => Some(Self::Quicktime),
            "pdf" => Some(Self::Pdf),
            "filename" => Some(Self::Filename),
            "mtime" => Some(Self::Mtime),
            _ => None,
        }
    }

    fn date_source(&self) -> DateSource {
        match self {
            Self::Exif => DateSource::Exif,
            Self::Quicktime => DateSource::Quicktime,
            Self::Pdf => DateSource::Pdf,
            Self::Filename => DateSource::Filename,
            Self::Mtime => DateSource::Mtime,
        }
    }
}

/// Walk `profile.metadata.date_sources` against `file`, returning the first
/// extractor to produce a date plus best-effort camera/lens/dimensions.
pub fn extract(profile: &Profile, file: &ScannedFile, kind: &str) -> Result<Metadata> {
    let path = file.absolute_path.as_path();
    let patterns = &profile.metadata.filename_date_patterns;

    // One extractor instance of each kind so per-path caches let the
    // chain of `try_*` calls open and parse the file once.
    let exif_extractor = exif::ExifExtractor::new();
    let qt_extractor = quicktime::QuicktimeExtractor::new();
    let pdf_extractor = pdf::PdfExtractor::new();

    let mut taken_at: Option<(NaiveDateTime, DateSource)> = None;
    let mut tried: Vec<String> = Vec::new();

    for raw in &profile.metadata.date_sources {
        let Some(spec) = SourceSpec::parse(raw) else {
            // Skip unparseable specs without recording them — `sources_tried`
            // should reflect what was actually attempted, not config typos.
            continue;
        };
        tried.push(raw.clone());
        let date = match &spec {
            SourceSpec::Exif => exif_extractor.try_date(path, kind)?,
            SourceSpec::Quicktime => qt_extractor.try_date(path, kind)?,
            SourceSpec::Pdf => pdf_extractor.try_date(path, kind)?,
            SourceSpec::Filename => {
                filename::FilenameExtractor::new(patterns.clone()).try_date(path, kind)?
            }
            SourceSpec::Mtime => mtime::MtimeExtractor.try_date(path, kind)?,
        };
        if let Some(dt) = date {
            taken_at = Some((dt, spec.date_source()));
            break;
        }
    }

    let Some((taken_at, taken_at_source)) = taken_at else {
        return Err(Error::NoDate {
            path: PathBuf::from(path),
            sources_tried: tried,
        });
    };

    let (camera, lens, dims) = if kind == "video" {
        (
            qt_extractor.try_camera(path, kind)?,
            qt_extractor.try_lens(path, kind)?,
            qt_extractor.try_dimensions(path, kind)?,
        )
    } else {
        (
            exif_extractor.try_camera(path, kind)?,
            exif_extractor.try_lens(path, kind)?,
            exif_extractor.try_dimensions(path, kind)?,
        )
    };
    let (width, height) = match dims {
        Some((w, h)) => (Some(w), Some(h)),
        None => (None, None),
    };

    let (author, title, vendor) = if is_document_kind(kind) {
        (
            pdf_extractor.try_author(path, kind)?,
            pdf_extractor.try_title(path, kind)?,
            pdf_extractor.try_vendor(path, kind)?,
        )
    } else {
        (None, None, None)
    };

    Ok(Metadata {
        taken_at,
        taken_at_source,
        camera,
        lens,
        kind: kind.to_string(),
        width,
        height,
        author,
        title,
        vendor,
    })
}

/// Whether a kind is document-shaped — i.e. worth running the PDF
/// extractor on. Profiles call PDF buckets different things; we accept
/// a small set of common synonyms.
fn is_document_kind(kind: &str) -> bool {
    matches!(
        kind,
        "document" | "documents" | "invoice" | "invoices" | "receipt" | "receipts" | "pdf"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_spec_parses_known_heads() {
        assert_eq!(SourceSpec::parse("exif"), Some(SourceSpec::Exif));
        assert_eq!(
            SourceSpec::parse("exif:DateTimeOriginal"),
            Some(SourceSpec::Exif)
        );
        assert_eq!(SourceSpec::parse("filename"), Some(SourceSpec::Filename));
        assert_eq!(SourceSpec::parse("mtime"), Some(SourceSpec::Mtime));
        assert_eq!(
            SourceSpec::parse("quicktime:CreationDate"),
            Some(SourceSpec::Quicktime)
        );
        assert_eq!(SourceSpec::parse("pdf:CreationDate"), Some(SourceSpec::Pdf));
    }

    #[test]
    fn source_spec_rejects_unknown() {
        assert_eq!(SourceSpec::parse("nope"), None);
        assert_eq!(SourceSpec::parse("xattr:Date"), None);
    }
}

// Dispatch tests live in-crate because `Profile` is `#[non_exhaustive]`.
#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::config::{Filters, Metadata as MetadataCfg};
    use std::io::Cursor;

    use ::exif::experimental::Writer;
    use ::exif::{Field, In, Tag, Value};
    use chrono::NaiveDate;

    fn build_exif_blob(fields: &[Field]) -> Vec<u8> {
        let mut writer = Writer::new();
        for f in fields {
            writer.push_field(f);
        }
        let mut buf = Cursor::new(Vec::new());
        writer.write(&mut buf, false).expect("exif write");
        buf.into_inner()
    }

    fn wrap_jpeg_with_exif(exif_blob: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(exif_blob.len() + 32);
        out.extend_from_slice(&[0xFF, 0xD8]);
        out.extend_from_slice(&[0xFF, 0xE1]);
        let seg_len: u16 = u16::try_from(2 + 6 + exif_blob.len()).expect("fits in u16");
        out.extend_from_slice(&seg_len.to_be_bytes());
        out.extend_from_slice(b"Exif\0\0");
        out.extend_from_slice(exif_blob);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    fn bare_jpeg() -> Vec<u8> {
        vec![0xFF, 0xD8, 0xFF, 0xD9]
    }

    fn ascii(s: &str) -> Value {
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        Value::Ascii(vec![bytes])
    }

    fn full_exif_jpeg() -> Vec<u8> {
        let dt = Field {
            tag: Tag::DateTimeOriginal,
            ifd_num: In::PRIMARY,
            value: ascii("2024:03:15 14:22:10"),
        };
        let make = Field {
            tag: Tag::Make,
            ifd_num: In::PRIMARY,
            value: ascii("Canon"),
        };
        let model = Field {
            tag: Tag::Model,
            ifd_num: In::PRIMARY,
            value: ascii("EOS R5"),
        };
        let lens = Field {
            tag: Tag::LensModel,
            ifd_num: In::PRIMARY,
            value: ascii("RF 24-70mm F2.8 L IS USM"),
        };
        wrap_jpeg_with_exif(&build_exif_blob(&[dt, make, model, lens]))
    }

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).expect("write fixture");
        p
    }

    fn photo_profile(date_sources: &[&str], patterns: &[&str]) -> Profile {
        Profile {
            inputs: vec![PathBuf::from("/in")],
            filters: Filters::default(),
            extensions: Default::default(),
            kinds: Default::default(),
            metadata: MetadataCfg {
                date_sources: date_sources.iter().map(|s| (*s).to_string()).collect(),
                filename_date_patterns: patterns.iter().map(|s| (*s).to_string()).collect(),
            },
            sequence: Default::default(),
            dedupe: Default::default(),
            health: Default::default(),
            templates: Default::default(),
            state: Default::default(),
            outputs: Vec::new(),
        }
    }

    fn scanned(root: &Path, name: &str) -> ScannedFile {
        ScannedFile {
            source_root: root.to_path_buf(),
            absolute_path: root.join(name),
            relative_path: PathBuf::from(name),
        }
    }

    #[test]
    fn dispatch_picks_exif_when_available() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "IMG_20990101_000000.jpg", &full_exif_jpeg());

        let profile = photo_profile(
            &["exif:DateTimeOriginal", "filename", "mtime"],
            &["IMG_%Y%m%d_%H%M%S"],
        );
        let file = scanned(tmp.path(), "IMG_20990101_000000.jpg");
        let md = extract(&profile, &file, "photo").unwrap();

        // EXIF wins over the filename's 2099 date.
        assert_eq!(md.taken_at_source, DateSource::Exif);
        assert_eq!(
            md.taken_at,
            NaiveDate::from_ymd_opt(2024, 3, 15)
                .unwrap()
                .and_hms_opt(14, 22, 10)
                .unwrap()
        );
        assert_eq!(md.camera.as_deref(), Some("Canon EOS R5"));
        assert_eq!(md.lens.as_deref(), Some("RF 24-70mm F2.8 L IS USM"));
        assert_eq!(md.kind, "photo");
    }

    #[test]
    fn dispatch_falls_through_to_filename() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "IMG_20240315_142210.jpg", &bare_jpeg());

        let profile = photo_profile(
            &["exif:DateTimeOriginal", "filename", "mtime"],
            &["IMG_%Y%m%d_%H%M%S"],
        );
        let file = scanned(tmp.path(), "IMG_20240315_142210.jpg");
        let md = extract(&profile, &file, "photo").unwrap();

        assert_eq!(md.taken_at_source, DateSource::Filename);
        assert_eq!(
            md.taken_at,
            NaiveDate::from_ymd_opt(2024, 3, 15)
                .unwrap()
                .and_hms_opt(14, 22, 10)
                .unwrap()
        );
        assert!(md.camera.is_none());
        assert!(md.lens.is_none());
    }

    #[test]
    fn dispatch_falls_through_to_mtime() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_file(tmp.path(), "screenshot.jpg", &bare_jpeg());

        // Pin mtime so the assertion is stable across runs.
        let ts_unix: i64 = 1_710_512_530;
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(ts_unix, 0)).unwrap();

        let profile = photo_profile(
            &["exif:DateTimeOriginal", "filename", "mtime"],
            &["IMG_%Y%m%d_%H%M%S"],
        );
        let file = scanned(tmp.path(), "screenshot.jpg");
        let md = extract(&profile, &file, "photo").unwrap();

        assert_eq!(md.taken_at_source, DateSource::Mtime);
        let expected = chrono::DateTime::from_timestamp(ts_unix, 0)
            .unwrap()
            .with_timezone(&chrono::Local)
            .naive_local();
        assert_eq!(md.taken_at, expected);
    }

    #[test]
    fn dispatch_errors_when_no_source_produces_date() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_file(tmp.path(), "screenshot.jpg", &bare_jpeg());

        let profile = photo_profile(
            // No mtime in the chain, and filename pattern doesn't match.
            &["exif:DateTimeOriginal", "filename"],
            &["IMG_%Y%m%d_%H%M%S"],
        );
        let file = scanned(tmp.path(), "screenshot.jpg");
        let err = extract(&profile, &file, "photo").unwrap_err();

        match err {
            Error::NoDate { sources_tried, .. } => {
                assert_eq!(
                    sources_tried,
                    vec!["exif:DateTimeOriginal".to_string(), "filename".to_string()]
                );
            }
            other => panic!("expected NoDate, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_picks_quicktime_for_video_with_creationdate() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bytes = super::quicktime_test_fixtures::iphone_like_mp4(
            "2024-03-15T14:22:10-07:00",
            Some("Apple"),
            Some("iPhone 13 Pro"),
        );
        write_file(tmp.path(), "VID_20990101_000000.mp4", &bytes);

        let profile = photo_profile(
            &["quicktime:CreationDate", "filename", "mtime"],
            &["VID_%Y%m%d_%H%M%S"],
        );
        let file = scanned(tmp.path(), "VID_20990101_000000.mp4");
        let md = extract(&profile, &file, "video").unwrap();

        // Quicktime wins over the filename's 2099 date.
        assert_eq!(md.taken_at_source, DateSource::Quicktime);
        assert_eq!(
            md.taken_at,
            NaiveDate::from_ymd_opt(2024, 3, 15)
                .unwrap()
                .and_hms_opt(14, 22, 10)
                .unwrap()
        );
        assert_eq!(md.camera.as_deref(), Some("Apple iPhone 13 Pro"));
        assert_eq!(md.kind, "video");
    }

    #[test]
    fn dispatch_picks_pdf_creation_date_over_filename_and_mtime() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bytes = super::pdf::test_fixtures::make_pdf(
            Some("D:20240315142210Z"),
            Some("Acme Corp"),
            Some("Invoice March"),
            Some("LibreOffice"),
        );
        // Filename carries a future date so a stray fallback is obvious.
        write_file(tmp.path(), "invoice_20990101.pdf", &bytes);

        let profile = photo_profile(
            &["pdf:CreationDate", "filename", "mtime"],
            &["invoice_%Y%m%d"],
        );
        let file = scanned(tmp.path(), "invoice_20990101.pdf");
        let md = extract(&profile, &file, "invoice").unwrap();

        assert_eq!(md.taken_at_source, DateSource::Pdf);
        assert_eq!(
            md.taken_at,
            NaiveDate::from_ymd_opt(2024, 3, 15)
                .unwrap()
                .and_hms_opt(14, 22, 10)
                .unwrap()
        );
        assert_eq!(md.author.as_deref(), Some("Acme Corp"));
        assert_eq!(md.title.as_deref(), Some("Invoice March"));
        assert_eq!(md.vendor.as_deref(), Some("Acme Corp"));
        assert_eq!(md.kind, "invoice");
    }

    #[test]
    fn dispatch_pdf_falls_back_to_filename_when_no_info() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bytes = super::pdf::test_fixtures::make_pdf_no_info();
        write_file(tmp.path(), "invoice_20240315.pdf", &bytes);

        let profile = photo_profile(
            &["pdf:CreationDate", "filename", "mtime"],
            &["invoice_%Y%m%d"],
        );
        let file = scanned(tmp.path(), "invoice_20240315.pdf");
        let md = extract(&profile, &file, "invoice").unwrap();

        assert_eq!(md.taken_at_source, DateSource::Filename);
        assert!(md.author.is_none());
        assert!(md.title.is_none());
    }
}
