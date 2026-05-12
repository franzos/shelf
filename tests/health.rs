//! End-to-end CLI tests for M12 `shelf health`.
//!
//! Each test stands up a tmpdir with a profile, optionally runs the
//! pipeline once to populate state, then invokes `shelf health` and
//! asserts on the output and exit code (4 when any entry is reported).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;

fn bin() -> Command {
    Command::cargo_bin("shelf").expect("binary `shelf` not found")
}

/// Photos profile that accepts `.jpg` and falls back to mtime — the same
/// shape used by `tests/cli.rs`, copied here so the two suites stay
/// independent.
fn photos_profile(input: &Path, output: &Path, state_db: &Path) -> String {
    format!(
        r#"
inputs = ["{input}"]

[filters]
include = ["*.jpg"]

[kinds]
photo = ["jpg"]

[metadata]
date_sources = ["mtime"]

[sequence]
scope = "day"
start = 1

[dedupe]
strategy = "sha256"
on_duplicate = "skip"
scope = "output"

[state]
database = "{state_db}"

[[output]]
name = "library"
path = "{output}"
mode = "copy"
on_conflict = "rename"
directory = "{{yyyy}}/{{mm}}"
filename  = "{{yyyy}}-{{mm}}-{{dd}}_{{seq:05}}"
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
    state_db: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        let input = tmp.path().join("in");
        let output = tmp.path().join("out");
        let state_db = tmp.path().join("state").join("photos.db");
        fs::create_dir_all(&config_dir).unwrap();
        fs::create_dir_all(&input).unwrap();
        let profile_path = config_dir.join("photos.toml");
        let toml = photos_profile(&input, &output, &state_db);
        fs::write(&profile_path, toml).unwrap();
        Self {
            _tmp: tmp,
            config_dir,
            input,
            output,
            state_db,
        }
    }

    fn stage_well_formed_jpeg(&self, name: &str, seed: u8) {
        // Minimal JPEG: SOI + APP0 marker padding + EOI. Body content
        // varies by seed so dedupe doesn't collapse multiple stagings.
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
        bytes.extend(std::iter::repeat_n(seed, 64));
        bytes.extend_from_slice(&[0xFF, 0xD9]);
        fs::write(self.input.join(name), bytes).unwrap();
    }

    fn run_once(&self) {
        bin()
            .env("SHELF_CONFIG_DIR", &self.config_dir)
            .args(["run", "photos"])
            .assert()
            .success();
    }

    fn placed_path(&self, name: &str) -> Option<PathBuf> {
        find_placed_with_basename(&self.output, name)
    }
}

#[test]
fn clean_library_reports_no_entries() {
    let fx = Fixture::new();
    // Empty input: no `files` rows, no orphans, no truncation, no drift.
    // Health should exit 0.
    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["health", "photos"])
        .assert()
        .success();
}

#[test]
fn truncated_jpeg_in_output_is_reported() {
    let fx = Fixture::new();
    fx.stage_well_formed_jpeg("a.jpg", 1);
    fx.run_once();

    // Mutate the placed copy: chop off the EOI marker so the structural
    // check fires.
    let placed = fx
        .placed_path("a.jpg")
        .or_else(|| find_any_jpg(&fx.output))
        .expect("placed jpg should exist after run");
    let mut bytes = fs::read(&placed).unwrap();
    let cut = bytes.len().saturating_sub(2);
    bytes.truncate(cut);
    fs::write(&placed, bytes).unwrap();

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["health", "photos"])
        .assert()
        .code(4)
        .stdout(contains("health: truncated"));
}

#[test]
fn orphan_in_output_tree_is_reported() {
    let fx = Fixture::new();
    fx.stage_well_formed_jpeg("a.jpg", 1);
    fx.run_once();

    // Drop a file the state DB never saw into the output tree.
    let orphan = fx.output.join("dropped.jpg");
    {
        let mut f = fs::File::create(&orphan).unwrap();
        f.write_all(&[0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xD9]).unwrap();
    }

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["health", "photos"])
        .assert()
        .code(4)
        .stdout(contains("health: orphan").and(contains("dropped.jpg")));
}

#[test]
fn mtime_fallback_file_is_reported_as_missing_date() {
    let fx = Fixture::new();
    fx.stage_well_formed_jpeg("a.jpg", 1);
    fx.run_once();

    // The photos profile uses `date_sources = ["mtime"]` exclusively, so
    // every placed file has `taken_at_source = 'mtime'`.
    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["health", "photos"])
        .assert()
        .code(4)
        .stdout(contains("health: missing-date"));
}

#[test]
fn drift_detected_after_source_mutation() {
    let fx = Fixture::new();
    fx.stage_well_formed_jpeg("a.jpg", 1);
    fx.run_once();

    // Mutate the source file (the input copy is what the `files` row
    // points to). Bump mtime so the cache key actually shifts — though
    // here we don't rerun, so the cache key doesn't matter.
    let source = fx.input.join("a.jpg");
    fs::write(&source, b"completely different content").unwrap();

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["health", "photos"])
        .assert()
        .code(4)
        .stdout(contains("health: drift"));
}

#[test]
fn sample_zero_disables_drift_check() {
    let fx = Fixture::new();
    fx.stage_well_formed_jpeg("a.jpg", 1);
    fx.run_once();

    // Mutate the source so drift would otherwise fire.
    let source = fx.input.join("a.jpg");
    fs::write(&source, b"different").unwrap();

    let out = bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["health", "photos", "--sample", "0"])
        .assert()
        // missing-date still fires, so exit is 4 — but no drift line.
        .code(4)
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        !text.contains("health: drift"),
        "drift must be skipped when --sample 0, got: {text}"
    );
}

#[test]
fn missing_state_db_is_not_an_error() {
    // Fresh profile, no run → DB doesn't exist yet. Health should exit
    // cleanly with no entries.
    let fx = Fixture::new();
    let _ = &fx.state_db;
    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["health", "photos"])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn find_placed_with_basename(root: &Path, basename: &str) -> Option<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(basename))
        })
}

fn find_any_jpg(root: &Path) -> Option<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .find(|p| {
            p.extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("jpg"))
        })
}
