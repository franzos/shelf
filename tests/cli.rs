//! End-to-end CLI tests for M10.
//!
//! These run the `shelf` binary against a tmpdir fixture (profile in
//! `$SHELF_CONFIG_DIR`, inputs and outputs under the same tmp). They cover:
//!
//! - `shelf plan` and `shelf run` against a small synthetic input tree.
//! - `--dry-run` leaves the destination untouched.
//! - `--strict` exits 3 when a profile produces an `unrouted` health entry.
//! - `shelf status` reports a row per profile.
//! - M16: `--from <PATH>` (repeatable) overrides `profile.inputs`.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;

fn bin() -> Command {
    Command::cargo_bin("shelf").expect("binary `shelf` not found")
}

/// Profile that accepts `.jpg` and falls back to mtime for the date. Good
/// enough for a CLI smoke test — no need to fabricate EXIF.
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

/// Profile whose single output only accepts `.jpg` files. Drop a `.gif`
/// into the input tree and the gif becomes an `unrouted` health entry.
fn strict_fixture_profile(input: &Path, output: &Path, state_db: &Path) -> String {
    format!(
        r#"
inputs = ["{input}"]

[filters]
include = ["*.jpg", "*.gif"]

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
kinds = ["photo"]
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
    fn new(profile_toml_for: fn(&Path, &Path, &Path) -> String, profile_name: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        let input = tmp.path().join("in");
        let output = tmp.path().join("out");
        let state_db = tmp.path().join("state").join(format!("{profile_name}.db"));
        fs::create_dir_all(&config_dir).unwrap();
        fs::create_dir_all(&input).unwrap();
        // Note: don't create `output` — exercise the applier's mkdir path.
        let profile_path = config_dir.join(format!("{profile_name}.toml"));
        let toml = profile_toml_for(&input, &output, &state_db);
        fs::write(&profile_path, toml).unwrap();
        Self {
            _tmp: tmp,
            config_dir,
            input,
            output,
            state_db,
        }
    }

    fn stage_jpegs(&self, names: &[&str]) {
        for (i, n) in names.iter().enumerate() {
            // Distinct bytes per file so dedupe doesn't collapse them.
            let body = vec![u8::try_from(i).unwrap_or(0) + 1; 64];
            fs::write(self.input.join(n), body).unwrap();
        }
    }

    fn stage_file(&self, name: &str, body: &[u8]) {
        fs::write(self.input.join(name), body).unwrap();
    }
}

#[test]
fn plan_subcommand_prints_place_lines() {
    let fx = Fixture::new(photos_profile, "photos");
    fx.stage_jpegs(&["a.jpg", "b.jpg"]);

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["plan", "photos"])
        .assert()
        .success()
        .stdout(
            contains("place\tlibrary\t")
                .and(contains("a.jpg"))
                .and(contains("b.jpg")),
        );
}

#[test]
fn run_places_files_into_output_tree() {
    let fx = Fixture::new(photos_profile, "photos");
    fx.stage_jpegs(&["a.jpg", "b.jpg"]);

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["run", "photos"])
        .assert()
        .success();

    // The output dir should now exist and contain placed files.
    assert!(fx.output.exists(), "output dir must exist after run");
    let placed = walk_files(&fx.output);
    assert_eq!(placed.len(), 2, "expected two placed files, got {placed:?}");
}

#[test]
fn dry_run_leaves_destination_empty() {
    let fx = Fixture::new(photos_profile, "photos");
    fx.stage_jpegs(&["a.jpg", "b.jpg"]);

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["run", "photos", "--dry-run"])
        .assert()
        .success();

    // Apply path never ran; the destination tree must remain untouched.
    // (It may not even exist — we don't create the output dir unless we
    // place a file.)
    if fx.output.exists() {
        let placed = walk_files(&fx.output);
        assert!(
            placed.is_empty(),
            "dry-run must not place files; found {placed:?}"
        );
    }
}

#[test]
fn dry_run_writes_no_placements_rows() {
    let fx = Fixture::new(photos_profile, "photos");
    fx.stage_jpegs(&["a.jpg"]);

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["run", "photos", "--dry-run"])
        .assert()
        .success();

    // The planner may write `files` rows (those describe what's on disk),
    // but the `placements` table must be empty under --dry-run.
    let count = placements_count(&fx.state_db);
    assert_eq!(count, 0, "dry-run must leave placements empty");
}

#[test]
fn plan_alias_matches_run_dry_run_output() {
    let fx = Fixture::new(photos_profile, "photos");
    fx.stage_jpegs(&["a.jpg"]);

    let plan_out = bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["plan", "photos"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let dry_run_out = bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["run", "photos", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(
        plan_out, dry_run_out,
        "plan should be a stable alias for run --dry-run"
    );
}

#[test]
fn strict_promotes_unrouted_to_exit_3() {
    let fx = Fixture::new(strict_fixture_profile, "photos");
    fx.stage_jpegs(&["a.jpg"]);
    fx.stage_file("ignored.gif", b"not a real gif but the filter accepts it");

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["run", "photos", "--dry-run", "--strict"])
        .assert()
        .code(3)
        .stdout(contains("health: unrouted").and(contains("ignored.gif")));
}

#[test]
fn without_strict_health_entry_still_exits_zero() {
    let fx = Fixture::new(strict_fixture_profile, "photos");
    fx.stage_jpegs(&["a.jpg"]);
    fx.stage_file("ignored.gif", b"x");

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["run", "photos", "--dry-run"])
        .assert()
        .success()
        .stdout(contains("health: unrouted"));
}

#[test]
fn status_lists_profile_row_with_counts_after_run() {
    let fx = Fixture::new(photos_profile, "photos");
    fx.stage_jpegs(&["a.jpg", "b.jpg"]);

    // Run once so the DB has rows to summarize.
    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["run", "photos"])
        .assert()
        .success();

    let out = bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    // Header + at least the photos row.
    assert!(text.contains("profile\tfiles\tplacements\tbytes\tlast_run"));
    assert!(text.contains("photos\t2\t2\t"), "got: {text}");
}

#[test]
fn status_lists_profile_without_db_with_dashes() {
    // Fresh profile, no run → status should still show the row with `-`
    // sentinels rather than erroring.
    let fx = Fixture::new(photos_profile, "photos");
    let _ = &fx.input; // keep paths alive
    let out = bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("photos\t-\t-\t-\t-"), "got: {text}");
}

// ---------------------------------------------------------------------------
// `--from <PATH>` overrides (M16)
// ---------------------------------------------------------------------------

#[test]
fn from_overrides_profile_inputs_for_plan() {
    // The profile's `inputs` points at fx.input (empty). Pointing `--from`
    // at a fresh directory should make the planner walk that one instead.
    let fx = Fixture::new(photos_profile, "photos");
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("alt.jpg"), vec![42_u8; 32]).unwrap();

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["plan", "photos", "--from"])
        .arg(outside.path())
        .assert()
        .success()
        .stdout(contains("alt.jpg"));
}

#[test]
fn from_is_repeatable_and_chains_paths() {
    // Two `--from` directories, each containing a distinct file. Plan
    // output should mention both.
    let fx = Fixture::new(photos_profile, "photos");
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    fs::write(dir_a.path().join("alpha.jpg"), vec![1_u8; 32]).unwrap();
    fs::write(dir_b.path().join("bravo.jpg"), vec![2_u8; 32]).unwrap();

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["plan", "photos", "--from"])
        .arg(dir_a.path())
        .arg("--from")
        .arg(dir_b.path())
        .assert()
        .success()
        .stdout(contains("alpha.jpg").and(contains("bravo.jpg")));
}

#[test]
fn from_accepts_relative_paths_via_canonicalization() {
    // Stage a file under `outside/`, then invoke shelf with a relative
    // `--from` pointing at it. The CLI should canonicalize against the
    // working directory we pass to assert_cmd.
    let fx = Fixture::new(photos_profile, "photos");
    let outside = tempfile::tempdir().unwrap();
    let nested = outside.path().join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("rel.jpg"), vec![3_u8; 32]).unwrap();

    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .current_dir(outside.path())
        .args(["plan", "photos", "--from", "nested"])
        .assert()
        .success()
        .stdout(contains("rel.jpg"));
}

#[test]
fn from_with_nonexistent_path_errors_cleanly() {
    let fx = Fixture::new(photos_profile, "photos");
    bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args([
            "plan",
            "photos",
            "--from",
            "/definitely/does/not/exist/here",
        ])
        .assert()
        .failure();
}

#[test]
fn without_from_profile_inputs_drive_the_scan() {
    // Stage files in the profile's declared `inputs` AND in an outside dir.
    // Without `--from`, only the profile's files should appear in the plan.
    let fx = Fixture::new(photos_profile, "photos");
    fx.stage_jpegs(&["in_a.jpg"]);
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("out_b.jpg"), vec![9_u8; 32]).unwrap();

    let out = bin()
        .env("SHELF_CONFIG_DIR", &fx.config_dir)
        .args(["plan", "photos"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("in_a.jpg"), "got: {text}");
    assert!(
        !text.contains("out_b.jpg"),
        "outside path should be ignored without --from; got: {text}"
    );
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

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

fn placements_count(db_path: &Path) -> i64 {
    if !db_path.exists() {
        return 0;
    }
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.query_row("SELECT COUNT(*) FROM placements", [], |r| r.get(0))
        .unwrap_or(0)
}
