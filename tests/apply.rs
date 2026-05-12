//! Integration tests for the applier.

use std::fs;
use std::io::Cursor;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use exif::experimental::Writer;
use exif::{Field, In, Tag, Value};
use filetime::FileTime;
use tempfile::TempDir;

use shelf::apply::apply;
use shelf::config::Profile;
use shelf::plan::{PlannedAction, plan};
use shelf::scan::ScannedFile;
use shelf::state::State;

fn write_file(dir: &Path, name: &str, body: &[u8], mtime_unix: i64) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, body).unwrap();
    filetime::set_file_mtime(&p, FileTime::from_unix_time(mtime_unix, 0)).unwrap();
    p
}

fn scanned(root: &Path, name: &str) -> ScannedFile {
    ScannedFile {
        source_root: root.to_path_buf(),
        absolute_path: root.join(name),
        relative_path: PathBuf::from(name),
    }
}

fn exif_jpeg(date: &str) -> Vec<u8> {
    let mut writer = Writer::new();
    let bytes = {
        let mut v = date.as_bytes().to_vec();
        v.push(0);
        v
    };
    let field = Field {
        tag: Tag::DateTimeOriginal,
        ifd_num: In::PRIMARY,
        value: Value::Ascii(vec![bytes]),
    };
    writer.push_field(&field);
    let mut buf = Cursor::new(Vec::new());
    writer.write(&mut buf, false).unwrap();
    let blob = buf.into_inner();

    let mut out = Vec::with_capacity(blob.len() + 32);
    out.extend_from_slice(&[0xFF, 0xD8]);
    out.extend_from_slice(&[0xFF, 0xE1]);
    let seg_len: u16 = u16::try_from(2 + 6 + blob.len()).unwrap();
    out.extend_from_slice(&seg_len.to_be_bytes());
    out.extend_from_slice(b"Exif\0\0");
    out.extend_from_slice(&blob);
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

fn single_output_profile(
    out: &Path,
    mode: &str,
    on_duplicate: &str,
    on_conflict: &str,
    dedupe_scope: &str,
) -> Profile {
    let toml_src = format!(
        r#"
inputs = ["/tmp/i"]

[kinds]
photo = ["jpg"]

[metadata]
date_sources = ["exif:DateTimeOriginal", "mtime"]

[sequence]
scope = "day"
start = 1

[dedupe]
strategy = "sha256"
on_duplicate = "{on_duplicate}"
scope = "{dedupe_scope}"

[[output]]
name = "lib"
path = "{}"
mode = "{mode}"
on_conflict = "{on_conflict}"
directory = "{{yyyy}}/{{mm}}"
filename  = "{{yyyy}}-{{mm}}-{{dd}}_{{seq:05}}"
"#,
        out.display(),
    );
    toml::from_str(&toml_src).unwrap()
}

fn single_output_profile_with_preserve(out: &Path, mode: &str, preserve_mtime: bool) -> Profile {
    let toml_src = format!(
        r#"
inputs = ["/tmp/i"]

[kinds]
photo = ["jpg"]

[metadata]
date_sources = ["exif:DateTimeOriginal", "mtime"]

[sequence]
scope = "day"
start = 1

[dedupe]
strategy = "sha256"
on_duplicate = "skip"
scope = "output"

[[output]]
name = "lib"
path = "{}"
mode = "{mode}"
on_conflict = "rename"
directory = "{{yyyy}}/{{mm}}"
filename  = "{{yyyy}}-{{mm}}-{{dd}}_{{seq:05}}"
preserve_mtime = {preserve_mtime}
"#,
        out.display(),
    );
    toml::from_str(&toml_src).unwrap()
}

fn stage_single_file(
    mode: &str,
) -> (
    TempDir,
    State,
    Profile,
    Vec<Result<ScannedFile, shelf::error::Error>>,
) {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("out");
    fs::create_dir_all(&in_dir).unwrap();
    write_file(&in_dir, "a.jpg", &exif_jpeg("2024:03:15 12:00:00"), 1);

    let profile = single_output_profile(&out, mode, "skip", "rename", "output");
    let state = State::open_in_memory().unwrap();
    let cands = vec![Ok(scanned(&in_dir, "a.jpg"))];
    (tmp, state, profile, cands)
}

fn placements_count(state: &State) -> i64 {
    state
        .conn()
        .query_row("SELECT COUNT(*) FROM placements", [], |r| r.get(0))
        .unwrap()
}

fn temp_files_in(dir: &Path) -> usize {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".shelf-tmp-"))
        .count()
}

/// Whole-second granularity avoids flakes on filesystems with second-resolution
/// mtimes (vfat, some network mounts).
fn mtime_unix(p: &Path) -> i64 {
    let m = fs::metadata(p).unwrap();
    FileTime::from_last_modification_time(&m).unix_seconds()
}

#[test]
fn copy_places_file_and_leaves_source_in_place() {
    let (_tmp, mut state, profile, cands) = stage_single_file("copy");
    let plan = plan(&mut state, &profile, cands.into_iter()).unwrap();
    let report = apply(&mut state, &profile, &plan, None).unwrap();

    assert_eq!(report.placed, 1);
    assert_eq!(report.failed.len(), 0);
    assert_eq!(placements_count(&state), 1);

    let dst = match &plan.actions[0] {
        PlannedAction::Place { dst, .. } => dst.clone(),
        other => panic!("expected Place, got {other:?}"),
    };
    assert!(dst.exists(), "dst should be on disk");
    let src = match &plan.actions[0] {
        PlannedAction::Place { src, .. } => src.clone(),
        _ => unreachable!(),
    };
    assert!(src.exists(), "copy must not remove source");

    assert_eq!(
        fs::read(&src).unwrap(),
        fs::read(&dst).unwrap(),
        "dst bytes must match src",
    );

    assert_eq!(
        temp_files_in(dst.parent().unwrap()),
        0,
        "temp sibling must be gone after a clean copy"
    );
}

#[test]
fn move_renames_when_same_filesystem_and_removes_source() {
    let (_tmp, mut state, profile, cands) = stage_single_file("move");
    let plan = plan(&mut state, &profile, cands.into_iter()).unwrap();
    let src = match &plan.actions[0] {
        PlannedAction::Place { src, .. } => src.clone(),
        other => panic!("expected Place, got {other:?}"),
    };
    let dst = match &plan.actions[0] {
        PlannedAction::Place { dst, .. } => dst.clone(),
        _ => unreachable!(),
    };

    let report = apply(&mut state, &profile, &plan, None).unwrap();
    assert_eq!(report.placed, 1);
    assert!(dst.exists());
    assert!(!src.exists(), "move must remove source");
    assert_eq!(placements_count(&state), 1);
}

#[test]
fn hardlink_creates_link_with_same_inode() {
    let (_tmp, mut state, profile, cands) = stage_single_file("hardlink");
    let plan = plan(&mut state, &profile, cands.into_iter()).unwrap();
    let report = apply(&mut state, &profile, &plan, None).unwrap();
    assert_eq!(report.placed, 1);

    let (src, dst) = match &plan.actions[0] {
        PlannedAction::Place { src, dst, .. } => (src.clone(), dst.clone()),
        other => panic!("expected Place, got {other:?}"),
    };
    let src_meta = fs::metadata(&src).unwrap();
    let dst_meta = fs::metadata(&dst).unwrap();
    assert_eq!(
        src_meta.ino(),
        dst_meta.ino(),
        "hardlinked inodes must match"
    );
}

#[test]
fn symlink_points_at_absolute_source() {
    let (_tmp, mut state, profile, cands) = stage_single_file("symlink");
    let plan = plan(&mut state, &profile, cands.into_iter()).unwrap();
    let report = apply(&mut state, &profile, &plan, None).unwrap();
    assert_eq!(report.placed, 1);

    let (src, dst) = match &plan.actions[0] {
        PlannedAction::Place { src, dst, .. } => (src.clone(), dst.clone()),
        other => panic!("expected Place, got {other:?}"),
    };
    let target = fs::read_link(&dst).unwrap();
    assert!(target.is_absolute(), "symlink target should be absolute");
    assert_eq!(
        fs::canonicalize(&target).unwrap(),
        fs::canonicalize(&src).unwrap()
    );
}

#[test]
fn skipconflict_leaves_existing_file_untouched() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("out");
    fs::create_dir_all(&in_dir).unwrap();
    fs::create_dir_all(out.join("2024/03")).unwrap();
    let existing = out.join("2024/03/2024-03-15_00001.jpg");
    fs::write(&existing, b"keep-me").unwrap();

    write_file(&in_dir, "a.jpg", &exif_jpeg("2024:03:15 12:00:00"), 1);
    let profile = single_output_profile(&out, "copy", "skip", "skip", "output");
    let mut state = State::open_in_memory().unwrap();
    let cands = vec![Ok(scanned(&in_dir, "a.jpg"))];

    let plan = plan(&mut state, &profile, cands.into_iter()).unwrap();
    let report = apply(&mut state, &profile, &plan, None).unwrap();

    assert_eq!(report.placed, 0);
    assert_eq!(report.skipped_conflict, 1);
    assert_eq!(
        fs::read(&existing).unwrap(),
        b"keep-me",
        "existing file must be untouched"
    );
    assert_eq!(placements_count(&state), 0);
}

#[test]
fn rename_places_second_file_at_suffixed_path() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("out");
    fs::create_dir_all(&in_dir).unwrap();
    fs::create_dir_all(out.join("2024/03")).unwrap();
    let existing = out.join("2024/03/2024-03-15_00001.jpg");
    fs::write(&existing, b"first").unwrap();

    write_file(&in_dir, "a.jpg", &exif_jpeg("2024:03:15 12:00:00"), 1);
    let profile = single_output_profile(&out, "copy", "skip", "rename", "output");
    let mut state = State::open_in_memory().unwrap();
    let cands = vec![Ok(scanned(&in_dir, "a.jpg"))];

    let plan = plan(&mut state, &profile, cands.into_iter()).unwrap();
    let report = apply(&mut state, &profile, &plan, None).unwrap();
    assert_eq!(report.placed, 1);

    let dst = match &plan.actions[0] {
        PlannedAction::Place { dst, .. } => dst.clone(),
        _ => unreachable!(),
    };
    assert!(
        dst.to_string_lossy().contains("_2"),
        "expected _2 suffix, got {dst:?}"
    );
    assert!(dst.exists());
    assert_eq!(fs::read(&existing).unwrap(), b"first");
}

#[test]
fn replace_overwrites_existing_and_records_single_placement() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("out");
    fs::create_dir_all(&in_dir).unwrap();
    fs::create_dir_all(out.join("2024/03")).unwrap();
    let existing = out.join("2024/03/2024-03-15_00001.jpg");
    fs::write(&existing, b"old-bytes").unwrap();

    let new_bytes = exif_jpeg("2024:03:15 12:00:00");
    write_file(&in_dir, "a.jpg", &new_bytes, 1);

    let profile = single_output_profile(&out, "copy", "skip", "replace", "output");
    let mut state = State::open_in_memory().unwrap();
    let cands = vec![Ok(scanned(&in_dir, "a.jpg"))];

    let plan = plan(&mut state, &profile, cands.into_iter()).unwrap();
    let report = apply(&mut state, &profile, &plan, None).unwrap();
    assert_eq!(report.replaced, 1);
    assert_eq!(report.placed, 0);

    let dst = match &plan.actions[0] {
        PlannedAction::Replace { dst, .. } => dst.clone(),
        other => panic!("expected Replace, got {other:?}"),
    };
    assert_eq!(dst, existing);
    assert_eq!(
        fs::read(&dst).unwrap(),
        new_bytes,
        "dst now carries the new bytes"
    );
    assert_eq!(placements_count(&state), 1);
}

#[test]
fn replace_evicts_old_placement_row_for_same_dst() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("out");
    fs::create_dir_all(&in_dir).unwrap();

    let bytes = exif_jpeg("2024:03:15 12:00:00");
    let src_a = write_file(&in_dir, "a.jpg", &bytes, 1);
    let src_b = write_file(&in_dir, "b.jpg", &bytes, 2);

    let profile = single_output_profile(&out, "copy", "replace", "rename", "output");
    let mut state = State::open_in_memory().unwrap();

    let plan_a = plan(
        &mut state,
        &profile,
        vec![Ok(scanned(&in_dir, "a.jpg"))].into_iter(),
    )
    .unwrap();
    let ra = apply(&mut state, &profile, &plan_a, None).unwrap();
    assert_eq!(ra.placed, 1);

    let dst_a = match &plan_a.actions[0] {
        PlannedAction::Place { dst, .. } => dst.clone(),
        _ => unreachable!(),
    };
    let file_id_a: i64 = state
        .conn()
        .query_row(
            "SELECT file_id FROM placements WHERE dest_path = ?1",
            rusqlite::params![dst_a.to_string_lossy().to_string()],
            |r| r.get(0),
        )
        .unwrap();

    let plan_b = plan(
        &mut state,
        &profile,
        vec![Ok(scanned(&in_dir, "b.jpg"))].into_iter(),
    )
    .unwrap();
    assert!(
        matches!(plan_b.actions[0], PlannedAction::Replace { .. }),
        "expected Replace via dedupe, got {:?}",
        plan_b.actions[0]
    );
    let rb = apply(&mut state, &profile, &plan_b, None).unwrap();
    assert_eq!(rb.replaced, 1);

    let surviving_file_id: i64 = state
        .conn()
        .query_row(
            "SELECT file_id FROM placements WHERE dest_path = ?1",
            rusqlite::params![dst_a.to_string_lossy().to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(
        surviving_file_id, file_id_a,
        "displaced row should be gone, B's row should be there"
    );
    let row_count: i64 = state
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM placements WHERE dest_path = ?1",
            rusqlite::params![dst_a.to_string_lossy().to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(row_count, 1);
    assert!(src_a.exists());
    assert!(src_b.exists());
}

#[test]
fn dedupe_skip_emits_skipped_counter() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("out");
    fs::create_dir_all(&in_dir).unwrap();

    let bytes = exif_jpeg("2024:03:15 12:00:00");
    write_file(&in_dir, "a.jpg", &bytes, 1);
    write_file(&in_dir, "b.jpg", &bytes, 2);

    let profile = single_output_profile(&out, "copy", "skip", "rename", "output");
    let mut state = State::open_in_memory().unwrap();
    let cands = vec![Ok(scanned(&in_dir, "a.jpg")), Ok(scanned(&in_dir, "b.jpg"))];

    let plan = plan(&mut state, &profile, cands.into_iter()).unwrap();
    let report = apply(&mut state, &profile, &plan, None).unwrap();

    assert_eq!(report.placed, 1);
    assert_eq!(report.skipped_duplicate, 1);
    assert_eq!(placements_count(&state), 1);
}

/// Mid-2017; pinned in the past so a fresh-mtime assertion can't accidentally
/// match the source.
const PINNED_SRC_MTIME: i64 = 1_500_000_000;

#[test]
fn copy_preserves_source_mtime_by_default() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("out");
    fs::create_dir_all(&in_dir).unwrap();
    write_file(
        &in_dir,
        "a.jpg",
        &exif_jpeg("2024:03:15 12:00:00"),
        PINNED_SRC_MTIME,
    );

    let profile = single_output_profile_with_preserve(&out, "copy", true);
    let mut state = State::open_in_memory().unwrap();
    let cands = vec![Ok(scanned(&in_dir, "a.jpg"))];
    let plan = plan(&mut state, &profile, cands.into_iter()).unwrap();
    let report = apply(&mut state, &profile, &plan, None).unwrap();
    assert_eq!(report.placed, 1);

    let dst = match &plan.actions[0] {
        PlannedAction::Place { dst, .. } => dst.clone(),
        _ => unreachable!(),
    };
    assert_eq!(
        mtime_unix(&dst),
        PINNED_SRC_MTIME,
        "copy mode with preserve_mtime=true should mirror the source mtime"
    );
}

#[test]
fn copy_with_preserve_mtime_false_does_not_carry_source_mtime() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("out");
    fs::create_dir_all(&in_dir).unwrap();
    write_file(
        &in_dir,
        "a.jpg",
        &exif_jpeg("2024:03:15 12:00:00"),
        PINNED_SRC_MTIME,
    );

    let profile = single_output_profile_with_preserve(&out, "copy", false);
    let mut state = State::open_in_memory().unwrap();
    let cands = vec![Ok(scanned(&in_dir, "a.jpg"))];
    let plan = plan(&mut state, &profile, cands.into_iter()).unwrap();
    let report = apply(&mut state, &profile, &plan, None).unwrap();
    assert_eq!(report.placed, 1);

    let dst = match &plan.actions[0] {
        PlannedAction::Place { dst, .. } => dst.clone(),
        _ => unreachable!(),
    };
    assert_ne!(
        mtime_unix(&dst),
        PINNED_SRC_MTIME,
        "preserve_mtime=false should leave a fresh mtime on the destination"
    );
}

#[test]
fn move_same_fs_preserves_mtime_for_free() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("out");
    fs::create_dir_all(&in_dir).unwrap();
    write_file(
        &in_dir,
        "a.jpg",
        &exif_jpeg("2024:03:15 12:00:00"),
        PINNED_SRC_MTIME,
    );

    let profile = single_output_profile_with_preserve(&out, "move", true);
    let mut state = State::open_in_memory().unwrap();
    let cands = vec![Ok(scanned(&in_dir, "a.jpg"))];
    let plan = plan(&mut state, &profile, cands.into_iter()).unwrap();
    apply(&mut state, &profile, &plan, None).unwrap();

    let dst = match &plan.actions[0] {
        PlannedAction::Place { dst, .. } => dst.clone(),
        _ => unreachable!(),
    };
    assert_eq!(mtime_unix(&dst), PINNED_SRC_MTIME);
}

#[test]
fn hardlink_inherits_source_mtime_through_inode() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("out");
    fs::create_dir_all(&in_dir).unwrap();
    write_file(
        &in_dir,
        "a.jpg",
        &exif_jpeg("2024:03:15 12:00:00"),
        PINNED_SRC_MTIME,
    );

    let profile = single_output_profile_with_preserve(&out, "hardlink", true);
    let mut state = State::open_in_memory().unwrap();
    let cands = vec![Ok(scanned(&in_dir, "a.jpg"))];
    let plan = plan(&mut state, &profile, cands.into_iter()).unwrap();
    apply(&mut state, &profile, &plan, None).unwrap();

    let dst = match &plan.actions[0] {
        PlannedAction::Place { dst, .. } => dst.clone(),
        _ => unreachable!(),
    };
    assert_eq!(mtime_unix(&dst), PINNED_SRC_MTIME);
}

#[test]
fn copy_to_unwritable_dest_fails_cleanly_with_no_partial_artifacts() {
    // mode 500 is ignored when running as root.
    if nix_is_root() {
        eprintln!("skipping: running as root, dir mode 500 doesn't block writes");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("out");
    fs::create_dir_all(&in_dir).unwrap();
    fs::create_dir_all(out.join("2024/03")).unwrap();
    write_file(&in_dir, "a.jpg", &exif_jpeg("2024:03:15 12:00:00"), 1);

    let profile = single_output_profile(&out, "copy", "skip", "rename", "output");
    let mut state = State::open_in_memory().unwrap();
    let cands = vec![Ok(scanned(&in_dir, "a.jpg"))];
    let plan = plan(&mut state, &profile, cands.into_iter()).unwrap();
    let dst_parent = match &plan.actions[0] {
        PlannedAction::Place { dst, .. } => dst.parent().unwrap().to_path_buf(),
        _ => unreachable!(),
    };

    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(&dst_parent).unwrap().permissions();
    perms.set_mode(0o500);
    fs::set_permissions(&dst_parent, perms).unwrap();

    let report = apply(&mut state, &profile, &plan, None).unwrap();

    // Re-open for write so tmpdir can clean up.
    let mut perms = fs::metadata(&dst_parent).unwrap().permissions();
    perms.set_mode(0o700);
    fs::set_permissions(&dst_parent, perms).unwrap();

    assert_eq!(report.placed, 0);
    assert_eq!(report.failed.len(), 1, "the action should land in failed");
    assert_eq!(
        temp_files_in(&dst_parent),
        0,
        "failed copy must not leave a temp file behind"
    );
    assert_eq!(placements_count(&state), 0);
}

fn nix_is_root() -> bool {
    std::env::var("USER").map(|u| u == "root").unwrap_or(false)
        || std::env::var("LOGNAME")
            .map(|u| u == "root")
            .unwrap_or(false)
}

/// End-to-end exercise of the EXDEV copy fallback for `mode = "move"`
/// (and, by analogy, revert's move-restore path).
///
/// **Not runnable in standard CI:** `tempfile` always lands on a single
/// filesystem so `fs::rename` succeeds inline and the fallback branch
/// is dead. Set `SHELF_TEST_XDEV_DIR` to a path on a **different
/// filesystem** from `$TMPDIR` (e.g. `/dev/shm/shelf-xdev` on most
/// Linuxes; or any path on a separately mounted volume) to enable.
///
/// `cargo test -- --ignored xdev_move_falls_back_to_copy_remove`
#[cfg(unix)]
#[test]
#[ignore]
fn xdev_move_falls_back_to_copy_remove() {
    let xdev_root = match std::env::var("SHELF_TEST_XDEV_DIR") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => {
            eprintln!(
                "set SHELF_TEST_XDEV_DIR to a path on a different filesystem \
                 from $TMPDIR to exercise the EXDEV fallback"
            );
            return;
        }
    };
    fs::create_dir_all(&xdev_root).expect("create SHELF_TEST_XDEV_DIR");
    let cleanup_root = xdev_root.clone();
    let in_dir = xdev_root.join("in");
    fs::create_dir_all(&in_dir).unwrap();

    let local = TempDir::new().unwrap();
    let out = local.path().join("out");

    {
        let probe_src = in_dir.join(".xdev_probe.src");
        fs::write(&probe_src, b"probe").unwrap();
        let probe_dst = local.path().join("xdev_probe.tmp");
        let same_fs = match fs::rename(&probe_src, &probe_dst) {
            Ok(()) => {
                let _ = fs::remove_file(&probe_dst);
                true
            }
            Err(e) => {
                let _ = fs::remove_file(&probe_src);
                !matches!(e.raw_os_error(), Some(18))
            }
        };
        if same_fs {
            eprintln!(
                "SHELF_TEST_XDEV_DIR={} appears to share a filesystem with the \
                 tempdir — fs::rename succeeded inline. Point it at a different mount.",
                xdev_root.display()
            );
            let _ = fs::remove_dir_all(&cleanup_root);
            return;
        }
    }

    write_file(
        &in_dir,
        "a.jpg",
        &exif_jpeg("2024:03:15 12:00:00"),
        PINNED_SRC_MTIME,
    );
    let src = in_dir.join("a.jpg");

    let profile = single_output_profile_with_preserve(&out, "move", true);
    let mut state = State::open_in_memory().unwrap();
    let cands = vec![Ok(scanned(&in_dir, "a.jpg"))];
    let plan_v = plan(&mut state, &profile, cands.into_iter()).unwrap();
    let report = apply(&mut state, &profile, &plan_v, None).unwrap();

    assert_eq!(
        report.placed, 1,
        "move with EXDEV must place the file; got failures: {:?}",
        report.failed
    );
    assert_eq!(report.failed.len(), 0);
    assert!(!src.exists(), "source must be removed after copy+remove");
    let dst = match &plan_v.actions[0] {
        PlannedAction::Place { dst, .. } => dst.clone(),
        _ => unreachable!(),
    };
    assert!(dst.exists());
    assert_eq!(
        mtime_unix(&dst),
        PINNED_SRC_MTIME,
        "EXDEV fallback must stamp dst with src's mtime — captured before unlink"
    );

    let _ = fs::remove_dir_all(&cleanup_root);
}

/// Full pipeline (scan-shaped input → plan → apply) on a non-UTF-8
/// source filename. The lossy DB key path (`state::path_to_string`)
/// must produce a valid plan and a successful apply. Guards
/// PERFORMANCE.md #4: the upcoming `Cow<str>` refactor must keep
/// behaving the same on non-UTF-8 inputs.
#[cfg(unix)]
#[test]
fn plan_and_apply_survive_non_utf8_source_filename() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("out");
    fs::create_dir_all(&in_dir).unwrap();

    let mut name_bytes: Vec<u8> = b"weird-".to_vec();
    name_bytes.extend_from_slice(&[0xFF, 0xFE]);
    name_bytes.extend_from_slice(b".jpg");
    let weird = OsStr::from_bytes(&name_bytes);
    let weird_abs = in_dir.join(weird);
    fs::write(&weird_abs, exif_jpeg("2024:03:15 12:00:00")).unwrap();
    filetime::set_file_mtime(&weird_abs, FileTime::from_unix_time(1, 0)).unwrap();

    let profile = single_output_profile(&out, "copy", "skip", "rename", "output");
    let mut state = State::open_in_memory().unwrap();
    let cand = ScannedFile {
        source_root: in_dir.clone(),
        absolute_path: weird_abs.clone(),
        relative_path: PathBuf::from(weird),
    };
    let plan_v = plan(&mut state, &profile, vec![Ok(cand)].into_iter()).unwrap();
    assert_eq!(
        plan_v.actions.len(),
        1,
        "non-UTF-8 path must still produce a plan"
    );

    let report = apply(&mut state, &profile, &plan_v, None).unwrap();
    assert_eq!(
        report.placed, 1,
        "apply must succeed for a non-UTF-8 source"
    );
    assert_eq!(report.failed.len(), 0);

    let dst = match &plan_v.actions[0] {
        PlannedAction::Place { dst, .. } => dst.clone(),
        other => panic!("expected Place, got {other:?}"),
    };
    assert!(dst.exists());
}

/// A multi-action plan where one mid-list action fails must commit the
/// surrounding actions cleanly. Guards the upcoming batched-transaction
/// change (PERFORMANCE.md #5) where a per-file failure must not roll
/// back unrelated work — and a rerun must pick up exactly the missing
/// rows. Without this, a future change could silently lose the placements
/// adjacent to a failure or duplicate them on rerun.
#[test]
fn multi_action_plan_partial_failure_commits_remainder_and_replays_cleanly() {
    if nix_is_root() {
        eprintln!("skipping: running as root, dir mode 500 doesn't block writes");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("out");
    fs::create_dir_all(&in_dir).unwrap();

    let months = ["01", "02", "03", "04", "05"];
    for (i, m) in months.iter().enumerate() {
        let date = format!("2024:{m}:10 12:00:00");
        let mut bytes = exif_jpeg(&date);
        bytes.push(u8::try_from(i + 1).unwrap());
        write_file(
            &in_dir,
            &format!("img_{m}.jpg"),
            &bytes,
            i64::try_from(i + 1).unwrap(),
        );
    }

    let profile = single_output_profile(&out, "copy", "skip", "rename", "output");
    let mut state = State::open_in_memory().unwrap();
    let cands: Vec<_> = months
        .iter()
        .map(|m| Ok(scanned(&in_dir, &format!("img_{m}.jpg"))))
        .collect();
    let plan_v = plan(&mut state, &profile, cands.into_iter()).unwrap();
    assert_eq!(plan_v.actions.len(), months.len());

    let blocked_dir = out.join("2024/03");
    fs::create_dir_all(&blocked_dir).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(&blocked_dir).unwrap().permissions();
    perms.set_mode(0o500);
    fs::set_permissions(&blocked_dir, perms).unwrap();

    let report = apply(&mut state, &profile, &plan_v, None).unwrap();

    let mut perms = fs::metadata(&blocked_dir).unwrap().permissions();
    perms.set_mode(0o700);
    fs::set_permissions(&blocked_dir, perms).unwrap();

    assert_eq!(
        report.placed, 4,
        "actions surrounding the failure must commit"
    );
    assert_eq!(report.failed.len(), 1, "exactly the blocked action fails");
    assert_eq!(placements_count(&state), 4);

    let failed_path = match &report.failed[0].action {
        PlannedAction::Place { dst, .. } => dst.clone(),
        other => panic!("expected the failure on a Place, got {other:?}"),
    };
    assert!(
        failed_path.starts_with(&blocked_dir),
        "the failure must be the 2024/03 action, got {failed_path:?}"
    );

    let cands_rerun: Vec<_> = months
        .iter()
        .map(|m| Ok(scanned(&in_dir, &format!("img_{m}.jpg"))))
        .collect();
    let plan_2 = plan(&mut state, &profile, cands_rerun.into_iter()).unwrap();
    let mut fresh_places: Vec<PathBuf> = plan_2
        .actions
        .iter()
        .filter_map(|a| match a {
            PlannedAction::Place { dst, .. } => Some(dst.clone()),
            _ => None,
        })
        .collect();
    fresh_places.sort();
    assert_eq!(
        fresh_places.len(),
        1,
        "rerun must produce exactly one fresh Place (the previously failed action), got {fresh_places:?}"
    );
    assert!(
        fresh_places[0].starts_with(&blocked_dir),
        "the fresh Place must be the previously blocked 2024/03 file, got {:?}",
        fresh_places[0]
    );

    let report_2 = apply(&mut state, &profile, &plan_2, None).unwrap();
    assert_eq!(report_2.placed, 1);
    assert_eq!(report_2.failed.len(), 0);
    assert_eq!(
        placements_count(&state),
        5,
        "after the recovery rerun, every file has exactly one placement row"
    );
}
