//! End-to-end CLI test for the M14 invoices profile.
//!
//! Drives `shelf run` against a tiny invoices profile pointed at a
//! synthesized PDF carrying a known `/Info` dict. Asserts that the file
//! lands at a path templated with `{author}` rendered from the PDF's
//! `/Author` field, demonstrating that the PDF extractor, the metadata
//! dispatch, and the template renderer agree end-to-end.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;

fn bin() -> Command {
    Command::cargo_bin("shelf").expect("binary `shelf` not found")
}

/// Build a minimal PDF carrying the requested Info dict fields. Mirrors
/// the in-crate fixture builder; duplicated rather than exported because
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

fn invoices_profile(input: &Path, output: &Path, state_db: &Path) -> String {
    format!(
        r#"
inputs = ["{input}"]

[filters]
include = ["*.pdf"]

[kinds]
invoice = ["pdf"]

[metadata]
date_sources = ["pdf:CreationDate", "filename", "mtime"]
filename_date_patterns = ["%Y-%m-%d", "%Y%m%d"]

[templates.fallbacks]
author = "unknown_vendor"

[sequence]
scope = "month"
start = 1

[dedupe]
strategy = "sha256"
on_duplicate = "skip"
scope = "output"

[state]
database = "{state_db}"

[[output]]
name = "archive"
path = "{output}"
mode = "copy"
on_conflict = "rename"
directory = "{{yyyy}}/{{mm}}"
filename  = "{{yyyy}}-{{mm}}-{{dd}}_{{author}}_{{seq:04}}"
"#,
        input = input.display(),
        output = output.display(),
        state_db = state_db.display(),
    )
}

struct Fixture {
    _tmp: TempDir,
    config_dir: PathBuf,
    input: PathBuf,
    output: PathBuf,
    _state_db: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        let input = tmp.path().join("in");
        let output = tmp.path().join("out");
        let state_db = tmp.path().join("state").join("invoices.db");
        fs::create_dir_all(&config_dir).unwrap();
        fs::create_dir_all(&input).unwrap();
        let profile_path = config_dir.join("invoices.toml");
        let toml = invoices_profile(&input, &output, &state_db);
        fs::write(&profile_path, toml).unwrap();
        Self {
            _tmp: tmp,
            config_dir,
            input,
            output,
            _state_db: state_db,
        }
    }
}

#[test]
fn run_places_invoice_pdf_with_author_in_filename() {
    let fx = Fixture::new();
    let bytes = make_pdf(
        Some("D:20240315142210Z"),
        Some("Acme Corp"),
        Some("March Invoice"),
        Some("LibreOffice"),
    );
    fs::write(fx.input.join("invoice.pdf"), bytes).unwrap();

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["run", "invoices"])
        .assert()
        .success();

    let placed = walk_files(&fx.output);
    assert_eq!(placed.len(), 1, "expected one placed file, got {placed:?}");
    let p = placed.into_iter().next().unwrap();
    let rel = p
        .strip_prefix(&fx.output)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    // Templated path: {yyyy}/{mm}/{yyyy}-{mm}-{dd}_{author}_{seq:04}.pdf
    // With "Acme Corp" slugified to "acme_corp" and seq=1, scope=month.
    assert_eq!(rel, "2024/03/2024-03-15_acme_corp_0001.pdf");
}

#[test]
fn run_falls_back_to_unknown_vendor_when_pdf_has_no_author() {
    let fx = Fixture::new();
    let bytes = make_pdf(Some("D:20240315142210Z"), None, None, None);
    // Filename must match a known date pattern so the date is stable even
    // though the PDF carries one too — we want a deterministic test, not
    // a check on the fallback ladder.
    fs::write(fx.input.join("2024-03-15.pdf"), bytes).unwrap();

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["run", "invoices"])
        .assert()
        .success();

    let placed = walk_files(&fx.output);
    assert_eq!(placed.len(), 1);
    let p = placed.into_iter().next().unwrap();
    let rel = p
        .strip_prefix(&fx.output)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(rel, "2024/03/2024-03-15_unknown_vendor_0001.pdf");
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.exists() {
        return out;
    }
    let walker = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok);
    for e in walker {
        if e.file_type().is_file() {
            out.push(e.path().to_path_buf());
        }
    }
    out
}
