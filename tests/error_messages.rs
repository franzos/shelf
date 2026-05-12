//! Snapshot tests pinning the human-facing `Display` output of representative
//! [`shelf::error::Error`] variants. The aim is to lock the format so future
//! changes to error rendering surface in review.
//!
//! Synthesizes `io::Error`s so the snapshot doesn't depend on libc's exact
//! message wording for any specific errno (`std`'s os-error display string
//! varies across libc versions and locales).

use std::io;
use std::path::PathBuf;

use shelf::error::{ApplyErrorKind, Error, ValidationError};

fn synth_io(kind: io::ErrorKind, msg: &str) -> io::Error {
    io::Error::new(kind, msg)
}

#[test]
fn profile_not_found_message() {
    let err = Error::ProfileNotFound {
        name: "photos".to_string(),
        path: PathBuf::from("/home/franz/.config/shelf/photos.toml"),
    };
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn profile_ambiguous_message() {
    let err = Error::ProfileAmbiguous {
        dir: PathBuf::from("/home/franz/.config/shelf"),
        count: 3,
        names: vec!["photos".into(), "videos".into(), "invoices".into()],
    };
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn validation_message_with_multiple_errors() {
    let err = Error::Validation {
        path: PathBuf::from("/home/franz/.config/shelf/photos.toml"),
        errors: vec![
            ValidationError::NoInputs,
            ValidationError::DuplicateOutputName("lib".to_string()),
            ValidationError::ExtensionInMultipleKinds {
                ext: "jpg".to_string(),
                first: "photo".to_string(),
                second: "raw".to_string(),
            },
            ValidationError::BadGlob {
                location: "filters.include".to_string(),
                pattern: "*[".to_string(),
                reason: "unclosed character class".to_string(),
            },
        ],
    };
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn io_message_names_offending_path() {
    let err = Error::Io {
        path: PathBuf::from("/var/lib/shelf/state.db"),
        source: synth_io(
            io::ErrorKind::PermissionDenied,
            "permission denied (os error 13)",
        ),
    };
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn apply_copy_message_names_stage_and_path() {
    let err = Error::Apply {
        kind: ApplyErrorKind::Copy,
        path: PathBuf::from("/library/photos/2024/03/2024-03-15_00042.jpg"),
        source: synth_io(io::ErrorKind::Other, "input/output error (os error 5)"),
    };
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn no_date_lists_sources_tried() {
    let err = Error::NoDate {
        path: PathBuf::from("/import/screenshot.jpg"),
        sources_tried: vec!["exif:DateTimeOriginal".to_string(), "filename".to_string()],
    };
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn template_error_names_location_and_template() {
    let err = Error::Template {
        location: "output `library`.directory".to_string(),
        template: "{yyyy".to_string(),
        reason: "unclosed `{` at byte 0".to_string(),
    };
    insta::assert_snapshot!(err.to_string());
}
