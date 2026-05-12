//! Integration tests for run history and revert.

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

fn photos_profile(input: &Path, output: &Path, state_db: &Path, mode: &str) -> String {
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
mode = "{mode}"
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
    fn new(mode: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        let input = tmp.path().join("in");
        let output = tmp.path().join("out");
        let state_db = tmp.path().join("state").join("photos.db");
        fs::create_dir_all(&config_dir).unwrap();
        fs::create_dir_all(&input).unwrap();
        let profile_path = config_dir.join("photos.toml");
        let toml = photos_profile(&input, &output, &state_db, mode);
        fs::write(&profile_path, toml).unwrap();
        Self {
            _tmp: tmp,
            config_dir,
            input,
            output,
            state_db,
        }
    }

    fn cmd(&self) -> Command {
        let mut c = bin();
        c.env("SHELF_CONFIG_DIR", &self.config_dir);
        c
    }

    fn stage_jpegs(&self, names: &[&str]) {
        for (i, n) in names.iter().enumerate() {
            let body = vec![u8::try_from(i).unwrap_or(0) + 1; 64];
            fs::write(self.input.join(n), body).unwrap();
        }
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.state_db).unwrap()
    }
}

fn placed_files(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .collect()
}

#[test]
fn run_creates_a_run_row_with_counts_on_finish() {
    let fx = Fixture::new("copy");
    fx.stage_jpegs(&["a.jpg", "b.jpg"]);

    fx.cmd().args(["run", "photos"]).assert().success();

    let conn = fx.conn();
    let (id, started, finished, placed, dry_run): (
        i64,
        String,
        Option<String>,
        i64,
        i64,
    ) = conn
        .query_row(
            "SELECT id, started_at, finished_at, placed, dry_run FROM runs ORDER BY id DESC LIMIT 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                ))
            },
        )
        .unwrap();
    assert!(id > 0);
    assert!(!started.is_empty());
    assert!(finished.is_some(), "finished_at should be set after run");
    assert_eq!(placed, 2);
    assert_eq!(dry_run, 0);
}

#[test]
fn dry_run_marks_row_as_dry_run_and_writes_no_placements() {
    let fx = Fixture::new("copy");
    fx.stage_jpegs(&["a.jpg"]);

    fx.cmd()
        .args(["run", "photos", "--dry-run"])
        .assert()
        .success();

    let conn = fx.conn();
    let dry: i64 = conn
        .query_row(
            "SELECT dry_run FROM runs ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dry, 1);

    let placements: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM placements WHERE dest_path NOT LIKE ':reserved:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(placements, 0);
}

#[test]
fn mid_run_crash_leaves_row_incomplete() {
    // Simulate a crash by opening the run row through the library API and
    // dropping the State before finish — bypasses the CLI so the failure
    // injection is clean.
    let fx = Fixture::new("copy");
    fx.stage_jpegs(&["a.jpg"]);

    let profile_toml = fs::read_to_string(fx.config_dir.join("photos.toml")).unwrap();
    let profile: shelf::config::Profile = toml::from_str(&profile_toml).unwrap();
    let mut state = shelf::state::State::open("photos", &profile).unwrap();
    let _id = state.open_run("photos", &[], false, false).unwrap();
    drop(state);

    let out = fx
        .cmd()
        .args(["runs", "photos"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("(incomplete)"), "got: {text}");
}

#[test]
fn runs_list_is_newest_first_and_respects_limit() {
    let fx = Fixture::new("copy");
    fx.stage_jpegs(&["a.jpg"]);

    for _ in 0..3 {
        fx.cmd()
            .args(["run", "photos", "--dry-run"])
            .assert()
            .success();
    }

    let out = fx
        .cmd()
        .args(["runs", "photos", "--limit", "2"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    // header + 2 rows
    assert_eq!(lines.len(), 3, "got: {text}");
    let first_id: i64 = lines[1].split('\t').next().unwrap().parse().unwrap();
    let second_id: i64 = lines[2].split('\t').next().unwrap().parse().unwrap();
    assert!(first_id > second_id, "newest first: {text}");
}

#[test]
fn runs_show_lists_placements_for_one_run() {
    let fx = Fixture::new("copy");
    fx.stage_jpegs(&["a.jpg", "b.jpg"]);
    fx.cmd().args(["run", "photos"]).assert().success();

    let conn = fx.conn();
    let run_id: i64 = conn
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();

    let out = fx
        .cmd()
        .args(["runs", "photos", &run_id.to_string()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("file_id"), "header: {text}");
    let body_lines = text.lines().skip(1).count();
    assert_eq!(body_lines, 2, "expected 2 placements, got: {text}");
    assert!(text.contains("\tcopy\t"), "op_mode column: {text}");
}

#[test]
fn revert_copy_deletes_dests_and_drops_placements() {
    let fx = Fixture::new("copy");
    fx.stage_jpegs(&["a.jpg", "b.jpg"]);
    fx.cmd().args(["run", "photos"]).assert().success();

    assert_eq!(placed_files(&fx.output).len(), 2);
    let run_id: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();

    fx.cmd()
        .args(["revert", "photos", &run_id.to_string()])
        .assert()
        .success();

    assert_eq!(placed_files(&fx.output).len(), 0, "dests must be gone");
    let placements: i64 = fx
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM placements WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(placements, 0);
}

#[test]
fn revert_move_restores_files_to_source() {
    let fx = Fixture::new("move");
    fx.stage_jpegs(&["a.jpg", "b.jpg"]);
    let src_a = fx.input.join("a.jpg");
    let src_b = fx.input.join("b.jpg");

    fx.cmd().args(["run", "photos"]).assert().success();
    assert!(!src_a.exists(), "move should have removed source");
    assert!(!src_b.exists());

    let run_id: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();

    fx.cmd()
        .args(["revert", "photos", &run_id.to_string()])
        .assert()
        .success();

    assert!(src_a.exists(), "source must be restored");
    assert!(src_b.exists());
    assert_eq!(placed_files(&fx.output).len(), 0);
}

#[test]
fn revert_move_creates_missing_source_parent_dir() {
    let fx = Fixture::new("move");
    fx.stage_jpegs(&["a.jpg"]);
    let src_a = fx.input.join("a.jpg");

    fx.cmd().args(["run", "photos"]).assert().success();
    fs::remove_dir_all(&fx.input).unwrap();

    let run_id: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();

    fx.cmd()
        .args(["revert", "photos", &run_id.to_string()])
        .assert()
        .success();
    assert!(src_a.exists(), "source path restored");
}

#[test]
fn revert_hardlink_deletes_the_link() {
    let fx = Fixture::new("hardlink");
    fx.stage_jpegs(&["a.jpg"]);
    fx.cmd().args(["run", "photos"]).assert().success();
    assert_eq!(placed_files(&fx.output).len(), 1);

    let run_id: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();

    fx.cmd()
        .args(["revert", "photos", &run_id.to_string()])
        .assert()
        .success();
    assert_eq!(placed_files(&fx.output).len(), 0);
    assert!(fx.input.join("a.jpg").exists(), "source must stay intact");
}

/// A revert that hits a mid-list filesystem failure must continue
/// processing the remaining placements. The failed step stays in the
/// `placements` table; the others are dropped as usual. Guards the
/// contract that revert's per-step error path doesn't short-circuit the
/// whole batch (the batched-commit perf change must preserve this).
#[cfg(unix)]
#[test]
fn revert_continues_past_a_mid_step_filesystem_failure() {
    use std::os::unix::fs::PermissionsExt;

    fn nix_is_root() -> bool {
        std::env::var("USER").map(|u| u == "root").unwrap_or(false)
            || std::env::var("LOGNAME")
                .map(|u| u == "root")
                .unwrap_or(false)
    }
    if nix_is_root() {
        eprintln!("skipping: running as root, dir mode 500 doesn't block unlink");
        return;
    }

    let fx = Fixture::new("copy");
    let months = ["01", "02", "03"];
    let staged_names: Vec<String> = months.iter().map(|m| format!("img_{m}.jpg")).collect();
    let mut now = 1_700_000_000_i64;
    for (i, name) in staged_names.iter().enumerate() {
        let body = vec![u8::try_from(i + 1).unwrap(); 64];
        fs::write(fx.input.join(name), body).unwrap();
        let mtime = chrono::NaiveDate::from_ymd_opt(2024, u32::try_from(i).unwrap() + 1, 10)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        filetime::set_file_mtime(
            fx.input.join(name),
            filetime::FileTime::from_unix_time(mtime, 0),
        )
        .unwrap();
        now += 1;
    }
    let _ = now;

    fx.cmd().args(["run", "photos"]).assert().success();
    let placed = placed_files(&fx.output);
    assert_eq!(placed.len(), 3);

    let run_id: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();

    let feb_parent = fx.output.join("2024/02");
    assert!(feb_parent.is_dir(), "expected 2024/02 to exist");
    let mut perms = fs::metadata(&feb_parent).unwrap().permissions();
    perms.set_mode(0o500);
    fs::set_permissions(&feb_parent, perms).unwrap();

    let out = fx
        .cmd()
        .args(["revert", "photos", &run_id.to_string()])
        .assert()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();

    let mut perms = fs::metadata(&feb_parent).unwrap().permissions();
    perms.set_mode(0o700);
    fs::set_permissions(&feb_parent, perms).unwrap();

    let reverted_lines = text.lines().filter(|l| l.starts_with("revert\t")).count();
    let error_lines = text.lines().filter(|l| l.starts_with("error\t")).count();
    assert_eq!(
        reverted_lines, 2,
        "two of the three placements must revert past the failure, got:\n{text}"
    );
    assert_eq!(
        error_lines, 1,
        "the blocked placement must surface as an error, got:\n{text}"
    );

    let surviving: i64 = fx
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM placements WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        surviving, 1,
        "the failed step's placement row must remain; the two successful ones must be gone"
    );

    let remaining_files = placed_files(&fx.output);
    assert_eq!(
        remaining_files.len(),
        1,
        "only the blocked file should remain on disk"
    );
    assert!(remaining_files[0].starts_with(&feb_parent));
}

#[test]
fn revert_refuses_on_drift_without_force() {
    let fx = Fixture::new("copy");
    fx.stage_jpegs(&["a.jpg"]);
    fx.cmd().args(["run", "photos"]).assert().success();

    let placed: PathBuf = placed_files(&fx.output).into_iter().next().unwrap();
    fs::write(&placed, b"tampered").unwrap();

    let run_id: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();

    fx.cmd()
        .args(["revert", "photos", &run_id.to_string()])
        .assert()
        .success()
        .stdout(contains("drift detected"));

    assert!(placed.exists());
    let cnt: i64 = fx
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM placements WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cnt, 1);
}

#[test]
fn revert_force_overrides_drift() {
    let fx = Fixture::new("copy");
    fx.stage_jpegs(&["a.jpg"]);
    fx.cmd().args(["run", "photos"]).assert().success();

    let placed: PathBuf = placed_files(&fx.output).into_iter().next().unwrap();
    fs::write(&placed, b"tampered").unwrap();

    let run_id: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();

    fx.cmd()
        .args(["revert", "photos", &run_id.to_string(), "--force"])
        .assert()
        .success();
    assert!(!placed.exists());
}

#[test]
fn revert_move_refuses_when_source_already_exists() {
    let fx = Fixture::new("move");
    fx.stage_jpegs(&["a.jpg"]);
    fx.cmd().args(["run", "photos"]).assert().success();

    fs::write(fx.input.join("a.jpg"), b"new occupant").unwrap();

    let run_id: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    fx.cmd()
        .args(["revert", "photos", &run_id.to_string()])
        .assert()
        .success()
        .stdout(contains("source already exists"));

    assert_eq!(
        fs::read(fx.input.join("a.jpg")).unwrap(),
        b"new occupant".to_vec(),
        "user file must not be overwritten without --force",
    );
}

#[test]
fn revert_force_overrides_move_source_exists() {
    let fx = Fixture::new("move");
    fx.stage_jpegs(&["a.jpg"]);
    let original = fs::read(fx.input.join("a.jpg")).unwrap();
    fx.cmd().args(["run", "photos"]).assert().success();
    fs::write(fx.input.join("a.jpg"), b"new occupant").unwrap();

    let run_id: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    fx.cmd()
        .args(["revert", "photos", &run_id.to_string(), "--force"])
        .assert()
        .success();
    assert_eq!(
        fs::read(fx.input.join("a.jpg")).unwrap(),
        original,
        "source restored over occupant under --force",
    );
}

#[test]
fn revert_warns_when_dest_already_missing() {
    let fx = Fixture::new("copy");
    fx.stage_jpegs(&["a.jpg"]);
    fx.cmd().args(["run", "photos"]).assert().success();

    let placed: PathBuf = placed_files(&fx.output).into_iter().next().unwrap();
    fs::remove_file(&placed).unwrap();

    let run_id: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    fx.cmd()
        .args(["revert", "photos", &run_id.to_string()])
        .assert()
        .success()
        .stdout(contains("warning").and(contains("dest missing")));

    let cnt: i64 = fx
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM placements WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cnt, 0);
}

#[test]
fn revert_refuses_dry_run_target() {
    let fx = Fixture::new("copy");
    fx.stage_jpegs(&["a.jpg"]);
    fx.cmd()
        .args(["run", "photos", "--dry-run"])
        .assert()
        .success();

    let run_id: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    fx.cmd()
        .args(["revert", "photos", &run_id.to_string()])
        .assert()
        .failure()
        .stderr(contains("dry-run"));
}

#[test]
fn revert_refuses_revert_kind_target() {
    let fx = Fixture::new("copy");
    fx.stage_jpegs(&["a.jpg"]);
    fx.cmd().args(["run", "photos"]).assert().success();
    let first: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    fx.cmd()
        .args(["revert", "photos", &first.to_string()])
        .assert()
        .success();

    let revert_id: i64 = fx
        .conn()
        .query_row(
            "SELECT id FROM runs WHERE kind = 'revert' ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    fx.cmd()
        .args(["revert", "photos", &revert_id.to_string()])
        .assert()
        .failure()
        .stderr(contains("is itself a revert"));
}

#[test]
fn revert_refuses_already_reverted_without_force() {
    let fx = Fixture::new("copy");
    fx.stage_jpegs(&["a.jpg"]);
    fx.cmd().args(["run", "photos"]).assert().success();
    let id: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    fx.cmd()
        .args(["revert", "photos", &id.to_string()])
        .assert()
        .success();

    fx.cmd()
        .args(["revert", "photos", &id.to_string()])
        .assert()
        .failure()
        .stderr(contains("already reverted"));
}

#[test]
fn revert_force_re_reverts_already_reverted_run() {
    let fx = Fixture::new("copy");
    fx.stage_jpegs(&["a.jpg"]);
    fx.cmd().args(["run", "photos"]).assert().success();
    let id: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    fx.cmd()
        .args(["revert", "photos", &id.to_string()])
        .assert()
        .success();
    fx.cmd()
        .args(["revert", "photos", &id.to_string(), "--force"])
        .assert()
        .success();
}

#[test]
fn revert_dry_run_prints_plan_and_changes_nothing() {
    let fx = Fixture::new("copy");
    fx.stage_jpegs(&["a.jpg"]);
    fx.cmd().args(["run", "photos"]).assert().success();
    let placed_before = placed_files(&fx.output);
    let id: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();

    fx.cmd()
        .args(["revert", "photos", &id.to_string(), "--dry-run"])
        .assert()
        .success()
        .stdout(contains("would delete"));

    let placed_after = placed_files(&fx.output);
    assert_eq!(placed_before, placed_after, "dry-run must not change disk");

    let revert_rows: i64 = fx
        .conn()
        .query_row("SELECT COUNT(*) FROM runs WHERE kind = 'revert'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(revert_rows, 0);
}

#[test]
fn revert_links_target_via_reverted_at_and_reverted_by() {
    let fx = Fixture::new("copy");
    fx.stage_jpegs(&["a.jpg"]);
    fx.cmd().args(["run", "photos"]).assert().success();
    let target: i64 = fx
        .conn()
        .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    fx.cmd()
        .args(["revert", "photos", &target.to_string()])
        .assert()
        .success();

    let (reverted_at, reverted_by): (Option<String>, Option<i64>) = fx
        .conn()
        .query_row(
            "SELECT reverted_at, reverted_by FROM runs WHERE id = ?1",
            [target],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(reverted_at.is_some());
    assert!(reverted_by.is_some());
}
