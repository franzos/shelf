//! Integration tests for the EXIF, filename, QuickTime, and PDF extractors.
//!
//! Dispatch-level tests live alongside `extract` in `src/metadata/mod.rs`
//! because [`shelf::config::Profile`] is `#[non_exhaustive]` and can't be
//! constructed from outside the crate. Keeping the per-extractor tests
//! here exercises the public extractor APIs against real on-disk files.
//!
//! Fixtures are synthesized at test time: a minimal JPEG wrapper (SOI +
//! APP1 EXIF segment + EOI) around an EXIF block built via the kamadak-exif
//! `Writer`, and a minimal PDF builder mirroring the one in
//! `src/metadata/pdf.rs`. Keeps the repo free of binary blobs and pins
//! fixture content to the code that asserts it. MP4 fixtures used by
//! in-crate tests are built by a private `quicktime_test_fixtures` module;
//! the cross-crate tests here only need the negative path (Quicktime
//! extractor on a non-MP4 file should decline cleanly).

use std::fmt::Write as _;
use std::fs;
use std::io::Cursor;
use std::path::Path;

use chrono::NaiveDate;
use exif::experimental::Writer;
use exif::{Field, In, Tag, Value};
use tempfile::TempDir;

use shelf::metadata::Extractor;
use shelf::metadata::exif::ExifExtractor;
use shelf::metadata::filename::FilenameExtractor;
use shelf::metadata::pdf::PdfExtractor;
use shelf::metadata::quicktime::QuicktimeExtractor;

// ---------------------------------------------------------------------------
// JPEG fixture builder
// ---------------------------------------------------------------------------

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
    out.extend_from_slice(&[0xFF, 0xD8]); // SOI
    out.extend_from_slice(&[0xFF, 0xE1]); // APP1 marker
    let seg_len: u16 = u16::try_from(2 + 6 + exif_blob.len()).expect("fits in u16");
    out.extend_from_slice(&seg_len.to_be_bytes());
    out.extend_from_slice(b"Exif\0\0");
    out.extend_from_slice(exif_blob);
    out.extend_from_slice(&[0xFF, 0xD9]); // EOI
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
    let blob = build_exif_blob(&[dt, make, model, lens]);
    wrap_jpeg_with_exif(&blob)
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let p = dir.join(name);
    fs::write(&p, bytes).expect("write fixture");
    p
}

// ---------------------------------------------------------------------------
// PDF fixture builder (mirrors src/metadata/pdf.rs::test_fixtures)
// ---------------------------------------------------------------------------

/// Build a minimal PDF carrying the requested Info dict fields. The byte
/// layout matches the in-crate fixture builder; duplicated here because
/// the in-crate one is `pub(crate)` and not visible to integration tests.
fn make_pdf(
    creation_date: Option<&str>,
    author: Option<&str>,
    title: Option<&str>,
    producer: Option<&str>,
) -> Vec<u8> {
    let mut info_body = String::new();
    if let Some(d) = creation_date {
        write!(info_body, "/CreationDate ({d}) ").unwrap();
    }
    if let Some(a) = author {
        write!(info_body, "/Author ({a}) ").unwrap();
    }
    if let Some(t) = title {
        write!(info_body, "/Title ({t}) ").unwrap();
    }
    if let Some(p) = producer {
        write!(info_body, "/Producer ({p}) ").unwrap();
    }

    let obj1 = "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n";
    let obj2 = "2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n";
    let obj3 = format!("3 0 obj\n<< {info_body}>>\nendobj\n");

    let header = "%PDF-1.4\n%\u{e2}\u{e3}\u{cf}\u{d3}\n";
    let mut body = String::new();
    body.push_str(header);
    let offset1 = body.len();
    body.push_str(obj1);
    let offset2 = body.len();
    body.push_str(obj2);
    let offset3 = body.len();
    body.push_str(&obj3);

    let xref_offset = body.len();
    write!(
        body,
        "xref\n0 4\n0000000000 65535 f \n{:010} 00000 n \n{:010} 00000 n \n{:010} 00000 n \n",
        offset1, offset2, offset3
    )
    .unwrap();
    write!(
        body,
        "trailer\n<< /Size 4 /Root 1 0 R /Info 3 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n"
    )
    .unwrap();

    body.into_bytes()
}

// ---------------------------------------------------------------------------
// EXIF extractor
// ---------------------------------------------------------------------------

#[test]
fn exif_extractor_reads_full_metadata() {
    let tmp = TempDir::new().unwrap();
    let path = write_file(tmp.path(), "full.jpg", &full_exif_jpeg());

    let ex = ExifExtractor::new();
    let dt = ex.try_date(&path, "photo").unwrap().unwrap();
    let expected = NaiveDate::from_ymd_opt(2024, 3, 15)
        .unwrap()
        .and_hms_opt(14, 22, 10)
        .unwrap();
    assert_eq!(dt, expected);

    let camera = ex.try_camera(&path, "photo").unwrap();
    assert_eq!(camera.as_deref(), Some("Canon EOS R5"));

    let lens = ex.try_lens(&path, "photo").unwrap();
    assert_eq!(lens.as_deref(), Some("RF 24-70mm F2.8 L IS USM"));
}

#[test]
fn exif_extractor_returns_none_on_no_exif() {
    let tmp = TempDir::new().unwrap();
    let path = write_file(tmp.path(), "bare.jpg", &bare_jpeg());

    let ex = ExifExtractor::new();
    assert!(ex.try_date(&path, "photo").unwrap().is_none());
    assert!(ex.try_camera(&path, "photo").unwrap().is_none());
    assert!(ex.try_lens(&path, "photo").unwrap().is_none());
}

#[test]
fn exif_extractor_returns_none_on_non_image() {
    let tmp = TempDir::new().unwrap();
    let path = write_file(tmp.path(), "random.txt", b"hello world\n");

    // Not a JPEG/HEIC/TIFF/etc. — InvalidFormat from kamadak-exif maps to
    // Ok(None), letting the dispatch chain move on.
    assert!(
        ExifExtractor::new()
            .try_date(&path, "photo")
            .unwrap()
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Filename extractor — extra case on top of in-module unit tests
// ---------------------------------------------------------------------------

#[test]
fn filename_extractor_parses_img_pattern_against_real_path() {
    let tmp = TempDir::new().unwrap();
    let path = write_file(tmp.path(), "IMG_20240315_142210.jpg", b"");

    let ex = FilenameExtractor::new(vec!["IMG_%Y%m%d_%H%M%S".into()]);
    let dt = ex.try_date(&path, "photo").unwrap().unwrap();
    let expected = NaiveDate::from_ymd_opt(2024, 3, 15)
        .unwrap()
        .and_hms_opt(14, 22, 10)
        .unwrap();
    assert_eq!(dt, expected);
}

// ---------------------------------------------------------------------------
// QuickTime extractor — negative path against a non-MP4
// ---------------------------------------------------------------------------

#[test]
fn quicktime_extractor_returns_none_on_jpeg() {
    let tmp = TempDir::new().unwrap();
    let path = write_file(tmp.path(), "actually.jpg", &full_exif_jpeg());

    // A JPEG passed to the QuickTime extractor (e.g. a misconfigured profile)
    // should decline rather than error — the dispatcher relies on this so a
    // chain like `["quicktime:CreationDate", "exif:DateTimeOriginal"]` walks
    // through cleanly for photo files too.
    let ex = QuicktimeExtractor::new();
    assert!(ex.try_date(&path, "photo").unwrap().is_none());
    assert!(ex.try_camera(&path, "photo").unwrap().is_none());
    assert!(ex.try_dimensions(&path, "photo").unwrap().is_none());
}

#[test]
fn quicktime_extractor_returns_none_on_random_bytes() {
    let tmp = TempDir::new().unwrap();
    let path = write_file(tmp.path(), "noise.bin", b"this is not a container at all");

    assert!(
        QuicktimeExtractor::new()
            .try_date(&path, "video")
            .unwrap()
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// PDF extractor
// ---------------------------------------------------------------------------

#[test]
fn pdf_extractor_reads_full_info_dict() {
    let tmp = TempDir::new().unwrap();
    let bytes = make_pdf(
        Some("D:20240315142210Z"),
        Some("Jane Doe"),
        Some("Invoice March"),
        Some("LibreOffice 24.2"),
    );
    let path = write_file(tmp.path(), "full.pdf", &bytes);

    let ex = PdfExtractor::new();
    let dt = ex.try_date(&path, "document").unwrap().unwrap();
    let expected = NaiveDate::from_ymd_opt(2024, 3, 15)
        .unwrap()
        .and_hms_opt(14, 22, 10)
        .unwrap();
    assert_eq!(dt, expected);

    assert_eq!(
        ex.try_author(&path, "document").unwrap().as_deref(),
        Some("Jane Doe")
    );
    assert_eq!(
        ex.try_title(&path, "document").unwrap().as_deref(),
        Some("Invoice March")
    );
    // Vendor prefers Author, present here.
    assert_eq!(
        ex.try_vendor(&path, "document").unwrap().as_deref(),
        Some("Jane Doe")
    );
}

#[test]
fn pdf_extractor_returns_none_on_jpeg() {
    let tmp = TempDir::new().unwrap();
    let path = write_file(tmp.path(), "not.pdf", &full_exif_jpeg());

    let ex = PdfExtractor::new();
    assert!(ex.try_date(&path, "document").unwrap().is_none());
    assert!(ex.try_author(&path, "document").unwrap().is_none());
    assert!(ex.try_title(&path, "document").unwrap().is_none());
    assert!(ex.try_vendor(&path, "document").unwrap().is_none());
}

#[test]
fn pdf_extractor_vendor_falls_back_to_producer() {
    let tmp = TempDir::new().unwrap();
    let bytes = make_pdf(
        Some("D:20240315142210Z"),
        None,
        None,
        Some("Acme Scanner Pro"),
    );
    let path = write_file(tmp.path(), "scanned.pdf", &bytes);

    let ex = PdfExtractor::new();
    assert!(ex.try_author(&path, "document").unwrap().is_none());
    assert_eq!(
        ex.try_vendor(&path, "document").unwrap().as_deref(),
        Some("Acme Scanner Pro")
    );
}
