//! End-to-end CLI tests for `shelf verify`.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use rusqlite::Connection;
use tempfile::TempDir;

fn bin() -> Command {
    Command::cargo_bin("shelf").expect("binary `shelf` not found")
}

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

    /// Minimal JPEG: SOI + APP0 padding + EOI. `seed` varies the body so
    /// dedupe doesn't collapse multiple stagings.
    fn stage_well_formed_jpeg(&self, name: &str, seed: u8) {
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

    fn health_rows(&self) -> Vec<(String, Option<String>)> {
        let conn = Connection::open(&self.state_db).unwrap();
        let mut stmt = conn
            .prepare("SELECT kind, detail FROM health ORDER BY id")
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }
}

#[test]
fn clean_library_verify_reports_no_drift() {
    let fx = Fixture::new();
    fx.stage_well_formed_jpeg("a.jpg", 1);
    fx.run_once();

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["verify", "photos", "--full"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty().or(predicate::str::contains("health:").not()));

    assert!(
        fx.health_rows().is_empty(),
        "expected no health rows on a clean library"
    );
}

#[test]
fn full_verify_detects_destination_drift() {
    let fx = Fixture::new();
    fx.stage_well_formed_jpeg("a.jpg", 1);
    fx.run_once();

    let placed = find_any_jpg(&fx.output).expect("expected one placed jpg");
    fs::write(&placed, b"completely different bytes").unwrap();

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["verify", "photos", "--full"])
        .assert()
        .code(4)
        .stdout(contains("health: drift").and(contains("expected=")));

    let rows = fx.health_rows();
    assert_eq!(rows.len(), 1, "expected one drift row, got {rows:?}");
    assert_eq!(rows[0].0, "drift");
    assert!(rows[0].1.as_deref().unwrap_or("").contains("expected="));
}

#[test]
fn missing_destination_reported_when_file_removed() {
    let fx = Fixture::new();
    fx.stage_well_formed_jpeg("a.jpg", 1);
    fx.run_once();

    let placed = find_any_jpg(&fx.output).expect("expected one placed jpg");
    fs::remove_file(&placed).unwrap();

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["verify", "photos", "--full"])
        .assert()
        .code(4)
        .stdout(contains("health: missing-destination"));

    let rows = fx.health_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "missing-destination");
}

#[test]
fn sample_one_covers_the_only_placement() {
    let fx = Fixture::new();
    fx.stage_well_formed_jpeg("a.jpg", 1);
    fx.run_once();

    let placed = find_any_jpg(&fx.output).expect("expected one placed jpg");
    fs::write(&placed, b"different bytes").unwrap();

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["verify", "photos", "--sample", "1"])
        .assert()
        .code(4)
        .stdout(contains("health: drift"));
}

#[test]
fn sample_zero_skips_verification() {
    let fx = Fixture::new();
    fx.stage_well_formed_jpeg("a.jpg", 1);
    fx.run_once();

    let placed = find_any_jpg(&fx.output).expect("expected one placed jpg");
    fs::write(&placed, b"different").unwrap();

    let out = bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["verify", "photos", "--sample", "0"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        !text.contains("health: drift"),
        "expected no drift line with --sample 0, got: {text}"
    );

    assert!(
        fx.health_rows().is_empty(),
        "expected no rows written with --sample 0"
    );
}

/// Sampling must only inspect the rows it picked. A drift on an
/// unsampled row must NOT surface — otherwise `--sample N` is silently
/// "always full". Seeds many placements, mutates only one, and asserts
/// that a small sample either misses the drift entirely (most of the
/// time) or finds exactly one drift row. Multiple iterations rule out
/// luck. This is the actual sampling guarantee.
#[test]
fn sample_does_not_report_unsampled_drift() {
    let fx = Fixture::new();
    let total = 50;
    for i in 0..total {
        fx.stage_well_formed_jpeg(&format!("img_{i:03}.jpg"), u8::try_from(i + 1).unwrap());
    }
    fx.run_once();

    let conn = Connection::open(&fx.state_db).unwrap();
    let placed_paths: Vec<String> = conn
        .prepare(
            "SELECT dest_path FROM placements \
             WHERE dest_path NOT LIKE ':reserved:%' \
             ORDER BY dest_path",
        )
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(placed_paths.len(), total);
    drop(conn);

    let drifted = PathBuf::from(&placed_paths[0]);
    fs::write(&drifted, b"drifted bytes").unwrap();

    let sample_size = 5_usize;
    let mut found_drift = false;
    let mut runs_with_zero_drift = 0;
    for _ in 0..20 {
        let conn = Connection::open(&fx.state_db).unwrap();
        conn.execute("DELETE FROM health", []).unwrap();
        drop(conn);

        let _ = bin()
            .env("SHELF_CONFIG_DIR", &fx.config_dir)
            .args(["verify", "photos", "--sample", &sample_size.to_string()])
            .assert();

        let rows = fx.health_rows();
        for (kind, _) in &rows {
            assert!(
                kind == "drift" || kind == "missing-destination",
                "verify must only report drift / missing-destination kinds, got {kind}"
            );
        }
        let drift_rows = rows.iter().filter(|(k, _)| k == "drift").count();
        assert!(
            drift_rows <= 1,
            "with only 1 mutated file in the library, at most 1 drift row per run; got {drift_rows}"
        );
        if drift_rows == 1 {
            found_drift = true;
        } else {
            runs_with_zero_drift += 1;
        }
    }

    assert!(
        found_drift,
        "with 20 runs of --sample {sample_size} over {total} files including \
         one drifted, the drifted row should hit the sample at least once"
    );
    assert!(
        runs_with_zero_drift > 0,
        "with --sample {sample_size} over {total} files, at least some runs \
         must skip the drifted row entirely — otherwise sampling silently \
         scans every row (the actual sampling-guarantee regression)"
    );
}

#[test]
fn full_and_sample_are_mutually_exclusive() {
    let fx = Fixture::new();
    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["verify", "photos", "--full", "--sample", "5"])
        .assert()
        .code(2)
        .stderr(contains("cannot be used with").or(contains("conflict")));
}

#[test]
fn verify_with_no_state_db_is_clean() {
    let fx = Fixture::new();
    let _ = &fx.state_db;
    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["verify", "photos", "--full"])
        .assert()
        .success();
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
