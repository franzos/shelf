//! Integration tests for the planner.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use exif::experimental::Writer;
use exif::{Field, In, Tag, Value};
use filetime::FileTime;
use tempfile::TempDir;

use shelf::config::Profile;
use shelf::plan::{HealthKind, Plan, PlannedAction, plan};
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

fn fanout_profile(out_a: &Path, out_b: &Path) -> Profile {
    let toml_src = format!(
        r#"
inputs = ["/tmp/i"]

[filters]

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
name = "archive"
path = "{}"
mode = "copy"
on_conflict = "rename"
directory = "{{yyyy}}/{{mm}}"
filename  = "{{yyyy}}-{{mm}}-{{dd}}_{{seq:05}}"

[[output]]
name = "curated"
path = "{}"
mode = "copy"
on_conflict = "rename"
directory = "{{yyyy}}"
filename  = "{{yyyy}}-{{mm}}-{{dd}}_{{seq:05}}"
"#,
        out_a.display(),
        out_b.display(),
    );
    toml::from_str(&toml_src).unwrap()
}

fn single_output_profile(
    out: &Path,
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
mode = "copy"
on_conflict = "{on_conflict}"
directory = "{{yyyy}}/{{mm}}"
filename  = "{{yyyy}}-{{mm}}-{{dd}}_{{seq:05}}"
"#,
        out.display(),
    );
    toml::from_str(&toml_src).unwrap()
}

fn count_places(plan: &Plan) -> usize {
    plan.actions
        .iter()
        .filter(|a| matches!(a, PlannedAction::Place { .. }))
        .count()
}

/// Stringify a [`PlannedAction`] for snapshotting, stripping the random
/// tempdir prefix so the snapshot is portable across hosts.
fn describe_action(a: &PlannedAction, tmp: &Path) -> String {
    let strip = |p: &Path| -> String {
        p.strip_prefix(tmp)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned()
    };
    match a {
        PlannedAction::Place {
            dst,
            output_name,
            seq,
            ..
        } => format!("Place output={output_name} seq={seq} dst={}", strip(dst)),
        PlannedAction::SkipDuplicate {
            output_name,
            existing_dst,
            ..
        } => format!(
            "SkipDuplicate output={output_name} existing={}",
            strip(existing_dst)
        ),
        PlannedAction::SkipConflict {
            output_name, dst, ..
        } => format!("SkipConflict output={output_name} dst={}", strip(dst)),
        PlannedAction::Replace {
            output_name,
            dst,
            seq,
            ..
        } => format!("Replace output={output_name} seq={seq} dst={}", strip(dst)),
        _ => "Unknown".to_string(),
    }
}

#[test]
fn fanout_emits_one_place_per_output() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out_a = tmp.path().join("a");
    let out_b = tmp.path().join("b");
    fs::create_dir_all(&in_dir).unwrap();

    write_file(&in_dir, "img.jpg", &exif_jpeg("2024:03:15 12:00:00"), 1);
    let profile = fanout_profile(&out_a, &out_b);
    let mut state = State::open_in_memory().unwrap();
    let candidates = vec![Ok(scanned(&in_dir, "img.jpg"))];

    let plan = plan(&mut state, &profile, candidates.into_iter(), None).unwrap();

    assert_eq!(count_places(&plan), 2, "one place per accepting output");
    let outputs: Vec<&str> = plan
        .actions
        .iter()
        .filter_map(|a| match a {
            PlannedAction::Place { output_name, .. } => Some(output_name.as_str()),
            _ => None,
        })
        .collect();
    assert!(outputs.contains(&"archive"));
    assert!(outputs.contains(&"curated"));
}

#[test]
fn mutually_exclusive_match_routes_each_file_once() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out_a = tmp.path().join("invoices");
    let out_b = tmp.path().join("photos");
    fs::create_dir_all(&in_dir).unwrap();

    write_file(
        &in_dir,
        "invoice-2024-03-15.jpg",
        &exif_jpeg("2024:03:15 12:00:00"),
        1,
    );
    write_file(&in_dir, "img_001.jpg", &exif_jpeg("2024:03:15 12:00:00"), 2);

    let toml_src = format!(
        r#"
inputs = ["/tmp/i"]

[kinds]
photo = ["jpg"]

[metadata]
date_sources = ["exif:DateTimeOriginal", "mtime"]

[sequence]
scope = "day"

[dedupe]
strategy = "sha256"
on_duplicate = "skip"
scope = "output"

[[output]]
name = "invoices"
path = "{}"
mode = "copy"
directory = "{{yyyy}}"
filename  = "{{yyyy}}-{{mm}}-{{dd}}_{{seq:05}}"
match = ["invoice-*"]

[[output]]
name = "photos"
path = "{}"
mode = "copy"
directory = "{{yyyy}}"
filename  = "{{yyyy}}-{{mm}}-{{dd}}_{{seq:05}}"
match = ["img_*"]
"#,
        out_a.display(),
        out_b.display(),
    );
    let profile: Profile = toml::from_str(&toml_src).unwrap();

    let mut state = State::open_in_memory().unwrap();
    let candidates = vec![
        Ok(scanned(&in_dir, "invoice-2024-03-15.jpg")),
        Ok(scanned(&in_dir, "img_001.jpg")),
    ];

    let plan = plan(&mut state, &profile, candidates.into_iter(), None).unwrap();

    let places: Vec<(&str, &str)> = plan
        .actions
        .iter()
        .filter_map(|a| match a {
            PlannedAction::Place {
                src, output_name, ..
            } => Some((
                src.file_name().unwrap().to_str().unwrap(),
                output_name.as_str(),
            )),
            _ => None,
        })
        .collect();

    assert_eq!(places.len(), 2);
    assert!(places.contains(&("invoice-2024-03-15.jpg", "invoices")));
    assert!(places.contains(&("img_001.jpg", "photos")));
}

#[test]
fn unmatched_file_lands_in_health_as_unrouted() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();

    write_file(&in_dir, "lonely.jpg", &exif_jpeg("2024:03:15 12:00:00"), 1);

    let toml_src = format!(
        r#"
inputs = ["/tmp/i"]

[kinds]
photo = ["jpg"]
video = ["mp4"]

[metadata]
date_sources = ["exif:DateTimeOriginal", "mtime"]

[[output]]
name = "videos-only"
path = "{}"
mode = "copy"
directory = "{{yyyy}}"
filename  = "{{yyyy}}-{{mm}}-{{dd}}_{{seq:05}}"
kinds = ["video"]
"#,
        out.display(),
    );
    let profile: Profile = toml::from_str(&toml_src).unwrap();
    let mut state = State::open_in_memory().unwrap();
    let candidates = vec![Ok(scanned(&in_dir, "lonely.jpg"))];

    let plan = plan(&mut state, &profile, candidates.into_iter(), None).unwrap();

    assert_eq!(plan.actions.len(), 0);
    assert!(
        plan.health
            .iter()
            .any(|h| h.kind == HealthKind::Unrouted && h.path.ends_with("lonely.jpg")),
        "expected an Unrouted health entry, got: {:?}",
        plan.health
    );
}

#[test]
fn dedupe_skip_emits_skipduplicate_for_second_identical_file() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();

    let bytes = exif_jpeg("2024:03:15 12:00:00");
    write_file(&in_dir, "a.jpg", &bytes, 1);
    write_file(&in_dir, "b.jpg", &bytes, 2);

    let profile = single_output_profile(&out, "skip", "rename", "output");
    let mut state = State::open_in_memory().unwrap();
    let candidates = vec![Ok(scanned(&in_dir, "a.jpg")), Ok(scanned(&in_dir, "b.jpg"))];

    let plan = plan(&mut state, &profile, candidates.into_iter(), None).unwrap();

    let placed: Vec<_> = plan
        .actions
        .iter()
        .filter(|a| matches!(a, PlannedAction::Place { .. }))
        .collect();
    let skipped: Vec<_> = plan
        .actions
        .iter()
        .filter(|a| matches!(a, PlannedAction::SkipDuplicate { .. }))
        .collect();

    assert_eq!(placed.len(), 1, "first file placed");
    assert_eq!(skipped.len(), 1, "second file skipped as duplicate");
}

#[test]
fn dedupe_keep_both_places_both_with_distinguishing_suffix() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();

    let bytes = exif_jpeg("2024:03:15 12:00:00");
    write_file(&in_dir, "a.jpg", &bytes, 1);
    write_file(&in_dir, "b.jpg", &bytes, 2);

    let profile = single_output_profile(&out, "keep-both", "rename", "output");
    let mut state = State::open_in_memory().unwrap();
    let candidates = vec![Ok(scanned(&in_dir, "a.jpg")), Ok(scanned(&in_dir, "b.jpg"))];

    let plan = plan(&mut state, &profile, candidates.into_iter(), None).unwrap();

    let places: Vec<&PathBuf> = plan
        .actions
        .iter()
        .filter_map(|a| match a {
            PlannedAction::Place { dst, .. } => Some(dst),
            _ => None,
        })
        .collect();

    assert_eq!(places.len(), 2, "keep-both places both files");
    assert_ne!(places[0], places[1], "destinations must differ");
    assert!(
        places.iter().any(|p| p.to_string_lossy().contains("_dup")),
        "expected a _dup suffix in {:?}",
        places
    );
}

#[test]
fn dedupe_replace_emits_replace_action() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();

    let bytes = exif_jpeg("2024:03:15 12:00:00");
    write_file(&in_dir, "a.jpg", &bytes, 1);
    write_file(&in_dir, "b.jpg", &bytes, 2);

    let profile = single_output_profile(&out, "replace", "rename", "output");
    let mut state = State::open_in_memory().unwrap();
    let candidates = vec![Ok(scanned(&in_dir, "a.jpg")), Ok(scanned(&in_dir, "b.jpg"))];

    let plan = plan(&mut state, &profile, candidates.into_iter(), None).unwrap();

    let replaces: Vec<_> = plan
        .actions
        .iter()
        .filter(|a| matches!(a, PlannedAction::Replace { .. }))
        .collect();
    let places: Vec<_> = plan
        .actions
        .iter()
        .filter(|a| matches!(a, PlannedAction::Place { .. }))
        .collect();
    assert_eq!(places.len(), 1, "first file placed");
    assert_eq!(replaces.len(), 1, "second file queues a replace");
}

#[test]
fn conflict_skip_emits_skipconflict_when_dst_exists() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();
    fs::create_dir_all(out.join("2024/03")).unwrap();
    fs::write(out.join("2024/03/2024-03-15_00001.jpg"), b"existing").unwrap();

    write_file(&in_dir, "a.jpg", &exif_jpeg("2024:03:15 12:00:00"), 1);

    let profile = single_output_profile(&out, "skip", "skip", "output");
    let mut state = State::open_in_memory().unwrap();
    let candidates = vec![Ok(scanned(&in_dir, "a.jpg"))];

    let plan = plan(&mut state, &profile, candidates.into_iter(), None).unwrap();

    let skipped: Vec<_> = plan
        .actions
        .iter()
        .filter(|a| matches!(a, PlannedAction::SkipConflict { .. }))
        .collect();
    assert_eq!(skipped.len(), 1, "skip-on-conflict emits SkipConflict");
}

#[test]
fn conflict_rename_appends_numeric_suffix() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();
    fs::create_dir_all(out.join("2024/03")).unwrap();
    fs::write(out.join("2024/03/2024-03-15_00001.jpg"), b"existing").unwrap();

    write_file(&in_dir, "a.jpg", &exif_jpeg("2024:03:15 12:00:00"), 1);

    let profile = single_output_profile(&out, "skip", "rename", "output");
    let mut state = State::open_in_memory().unwrap();
    let candidates = vec![Ok(scanned(&in_dir, "a.jpg"))];

    let plan = plan(&mut state, &profile, candidates.into_iter(), None).unwrap();

    let dst = match &plan.actions[0] {
        PlannedAction::Place { dst, .. } => dst.clone(),
        other => panic!("expected Place, got {other:?}"),
    };
    assert!(
        dst.to_string_lossy().contains("2024-03-15_00001_2"),
        "expected _2 suffix in {dst:?}"
    );
}

#[test]
fn conflict_hash_suffix_appends_hash() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();
    fs::create_dir_all(out.join("2024/03")).unwrap();
    fs::write(out.join("2024/03/2024-03-15_00001.jpg"), b"existing").unwrap();

    write_file(&in_dir, "a.jpg", &exif_jpeg("2024:03:15 12:00:00"), 1);

    let profile = single_output_profile(&out, "skip", "hash-suffix", "output");
    let mut state = State::open_in_memory().unwrap();
    let candidates = vec![Ok(scanned(&in_dir, "a.jpg"))];

    let plan = plan(&mut state, &profile, candidates.into_iter(), None).unwrap();

    let dst = match &plan.actions[0] {
        PlannedAction::Place { dst, .. } => dst.clone(),
        other => panic!("expected Place, got {other:?}"),
    };
    let name = dst.file_stem().unwrap().to_string_lossy();
    assert!(
        name.contains("2024-03-15_00001_"),
        "expected hash suffix on stem, got {name}"
    );
    let suffix = name.rsplit_once('_').unwrap().1;
    assert_eq!(suffix.len(), 8, "hash suffix is 8 chars");
    assert!(suffix.bytes().all(|b| b.is_ascii_hexdigit()));
}

#[test]
fn conflict_replace_emits_replace_action() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();
    fs::create_dir_all(out.join("2024/03")).unwrap();
    fs::write(out.join("2024/03/2024-03-15_00001.jpg"), b"existing").unwrap();

    write_file(&in_dir, "a.jpg", &exif_jpeg("2024:03:15 12:00:00"), 1);

    let profile = single_output_profile(&out, "skip", "replace", "output");
    let mut state = State::open_in_memory().unwrap();
    let candidates = vec![Ok(scanned(&in_dir, "a.jpg"))];

    let plan = plan(&mut state, &profile, candidates.into_iter(), None).unwrap();

    assert!(matches!(plan.actions[0], PlannedAction::Replace { .. }));
}

#[test]
fn plan_is_deterministic_across_walker_orders() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();

    write_file(&in_dir, "z.jpg", &exif_jpeg("2024:03:15 09:00:00"), 1);
    write_file(&in_dir, "a.jpg", &exif_jpeg("2024:03:15 12:00:00"), 2);
    write_file(&in_dir, "m.jpg", &exif_jpeg("2024:03:15 15:00:00"), 3);

    let profile = single_output_profile(&out, "skip", "rename", "output");

    let order_one = vec![
        Ok(scanned(&in_dir, "z.jpg")),
        Ok(scanned(&in_dir, "a.jpg")),
        Ok(scanned(&in_dir, "m.jpg")),
    ];
    let order_two = vec![
        Ok(scanned(&in_dir, "m.jpg")),
        Ok(scanned(&in_dir, "a.jpg")),
        Ok(scanned(&in_dir, "z.jpg")),
    ];

    let mut s1 = State::open_in_memory().unwrap();
    let p1 = plan(&mut s1, &profile, order_one.into_iter(), None).unwrap();
    let mut s2 = State::open_in_memory().unwrap();
    let p2 = plan(&mut s2, &profile, order_two.into_iter(), None).unwrap();

    fn normalize(plan: &Plan) -> Vec<(String, PathBuf, u64)> {
        let mut v: Vec<_> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                PlannedAction::Place {
                    dst,
                    output_name,
                    seq,
                    ..
                } => Some((output_name.clone(), dst.clone(), *seq)),
                _ => None,
            })
            .collect();
        v.sort();
        v
    }
    assert_eq!(normalize(&p1), normalize(&p2));
}

#[test]
fn rerun_against_same_state_yields_same_plan() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();

    write_file(&in_dir, "a.jpg", &exif_jpeg("2024:03:15 09:00:00"), 1);
    write_file(&in_dir, "b.jpg", &exif_jpeg("2024:03:15 10:00:00"), 2);

    let profile = single_output_profile(&out, "skip", "rename", "output");
    let mut state = State::open_in_memory().unwrap();
    let cands_a = vec![Ok(scanned(&in_dir, "a.jpg")), Ok(scanned(&in_dir, "b.jpg"))];
    let cands_b = vec![Ok(scanned(&in_dir, "a.jpg")), Ok(scanned(&in_dir, "b.jpg"))];

    let p1 = plan(&mut state, &profile, cands_a.into_iter(), None).unwrap();
    let p2 = plan(&mut state, &profile, cands_b.into_iter(), None).unwrap();

    let dsts = |plan: &Plan| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                PlannedAction::Place { dst, .. } => Some(dst.clone()),
                _ => None,
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(dsts(&p1), dsts(&p2));
}

#[test]
fn planner_does_not_write_placements_rows() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();
    write_file(&in_dir, "a.jpg", &exif_jpeg("2024:03:15 12:00:00"), 1);

    let profile = single_output_profile(&out, "skip", "rename", "output");
    let mut state = State::open_in_memory().unwrap();
    let cands = vec![Ok(scanned(&in_dir, "a.jpg"))];
    let _ = plan(&mut state, &profile, cands.into_iter(), None).unwrap();

    let count: i64 = state
        .conn()
        .query_row("SELECT COUNT(*) FROM placements", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0, "planner must not write to placements");
}

#[test]
fn planner_health_entry_for_walker_error() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("o");
    let profile = single_output_profile(&out, "skip", "rename", "output");
    let mut state = State::open_in_memory().unwrap();

    let err = shelf::error::Error::PathStripPrefix {
        root: PathBuf::from("/x"),
        path: PathBuf::from("/y/z"),
    };
    let cands = vec![Err::<ScannedFile, _>(err)];
    let p = plan(&mut state, &profile, cands.into_iter(), None).unwrap();
    assert!(p.actions.is_empty());
    assert!(
        p.health.iter().any(|h| h.kind == HealthKind::WalkError),
        "expected WalkError health, got {:?}",
        p.health
    );
}

/// Once a file has been placed with `seq = N`, re-planning that same
/// file under `keep-both` dedupe must reuse `N` for the seq sentinel
/// (the new placement lands at `<dst-with-seq-N>_dup1.jpg`). Guards
/// the upcoming bulk preload at planner start (PERFORMANCE.md #3): a
/// stale or mis-keyed `HashMap<(FileId, output), seq>` would advance
/// to the next free seq instead of reusing the prior one.
/// Determinism contract for parallel hashing (PERFORMANCE.md #6). The
/// pure-compute phase will run concurrently and stream results back in
/// arbitrary order; the planner sorts after, so the produced plan must
/// be identical regardless of input order. This test shuffles a mixed
/// fixture (photos with EXIF dates, files that fall through to mtime,
/// duplicates, multiple dates) across many permutations and asserts a
/// single normalised output.
#[test]
fn plan_is_deterministic_across_many_input_orderings() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();

    let mut names = Vec::new();
    for (i, date) in [
        "2024:03:15 09:00:00",
        "2024:03:15 09:00:00",
        "2024:03:15 12:00:00",
        "2024:03:16 09:00:00",
        "2024:03:17 09:00:00",
        "2024:04:01 12:00:00",
    ]
    .iter()
    .enumerate()
    {
        let mut bytes = exif_jpeg(date);
        bytes.push(u8::try_from(i + 1).unwrap());
        let name = format!("exif_{i}.jpg");
        write_file(&in_dir, &name, &bytes, i64::try_from(i + 1).unwrap());
        names.push(name);
    }
    for i in 0..4 {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
        bytes.extend(std::iter::repeat_n(u8::try_from(0x20 + i).unwrap(), 32));
        bytes.extend_from_slice(&[0xFF, 0xD9]);
        let name = format!("noexif_{i}.jpg");
        let mtime = 1_700_000_000 + i64::from(i) * 3600;
        write_file(&in_dir, &name, &bytes, mtime);
        names.push(name);
    }

    let profile = single_output_profile(&out, "skip", "rename", "output");

    fn normalize(plan: &Plan) -> Vec<String> {
        let mut v: Vec<String> = plan
            .actions
            .iter()
            .map(|a| match a {
                PlannedAction::Place {
                    output_name,
                    seq,
                    dst,
                    sha256_hex,
                    ..
                } => format!(
                    "place|out={output_name}|seq={seq}|sha={}|dst={}",
                    &sha256_hex[..8],
                    dst.file_name().unwrap().to_string_lossy()
                ),
                PlannedAction::SkipDuplicate {
                    output_name,
                    existing_dst,
                    ..
                } => format!(
                    "dup|out={output_name}|existing={}",
                    existing_dst.file_name().unwrap().to_string_lossy()
                ),
                PlannedAction::SkipConflict {
                    output_name, dst, ..
                } => format!(
                    "conflict|out={output_name}|dst={}",
                    dst.file_name().unwrap().to_string_lossy()
                ),
                PlannedAction::Replace {
                    output_name,
                    seq,
                    dst,
                    ..
                } => format!(
                    "replace|out={output_name}|seq={seq}|dst={}",
                    dst.file_name().unwrap().to_string_lossy()
                ),
                _ => "unknown".to_string(),
            })
            .collect();
        v.sort();
        v
    }

    let mut baseline: Option<Vec<String>> = None;
    let permutations: [&[usize]; 4] = [
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        &[9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
        &[5, 2, 7, 0, 9, 4, 1, 6, 3, 8],
        &[7, 0, 1, 8, 3, 9, 2, 6, 5, 4],
    ];
    for order in permutations {
        let cands: Vec<_> = order
            .iter()
            .map(|i| Ok(scanned(&in_dir, &names[*i])))
            .collect();
        let mut state = State::open_in_memory().unwrap();
        let p = plan(&mut state, &profile, cands.into_iter(), None).unwrap();
        let normalized = normalize(&p);
        if let Some(b) = &baseline {
            assert_eq!(
                &normalized, b,
                "plan must be byte-identical across input orderings; \
                 differed on permutation {order:?}"
            );
        } else {
            baseline = Some(normalized);
        }
    }
}

fn run_batch_boundary_case(n: usize) {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();

    let bytes_template = exif_jpeg("2024:03:15 09:00:00");
    for i in 0..n {
        let mut bytes = bytes_template.clone();
        bytes.push(u8::try_from(i & 0xff).unwrap());
        bytes.push(u8::try_from((i >> 8) & 0xff).unwrap());
        bytes.push(u8::try_from((i >> 16) & 0xff).unwrap());
        let name = format!("img_{i:05}.jpg");
        write_file(&in_dir, &name, &bytes, i64::try_from(i + 1).unwrap());
    }

    let profile = single_output_profile(&out, "skip", "rename", "output");
    let mut state = State::open_in_memory().unwrap();

    let cands: Vec<_> = (0..n)
        .map(|i| Ok(scanned(&in_dir, &format!("img_{i:05}.jpg"))))
        .collect();
    let p = plan(&mut state, &profile, cands.into_iter(), None).unwrap();
    let report = shelf::apply::apply(&mut state, &profile, &p, None, None).unwrap();
    assert_eq!(report.placed, n as u64, "n={n}: placed count");
    assert_eq!(report.failed.len(), 0, "n={n}: zero failures");

    let count: i64 = state
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM placements WHERE dest_path NOT LIKE ':reserved:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, n as i64, "n={n}: every file landed in placements");

    let files_count: i64 = state
        .conn()
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(files_count, n as i64, "n={n}: every file landed in files");
}

/// Small-boundary smoke test for the planner's per-file loop: every
/// file must end up in `placements` regardless of exact count. Covers
/// the same off-by-one shape that the upcoming 1000-file batching
/// (PERFORMANCE.md #5) will introduce, at a size that fits the
/// regular test suite. The full 999/1000/1001 sweep lives in
/// [`placements_persist_across_real_batch_boundary`] (`#[ignore]`d
/// for runtime; run with `cargo test -- --ignored` once batching ships).
#[test]
fn placements_persist_across_small_boundary_sizes() {
    for &n in &[99_usize, 100, 101] {
        run_batch_boundary_case(n);
    }
}

/// Full-fat boundary test against the actual 1000-file batch size
/// landing in PERFORMANCE.md #5. `#[ignore]`d because it takes tens
/// of seconds in debug mode; run on CI and pre-merge of the batching
/// change with `cargo test -- --ignored`.
#[test]
#[ignore]
fn placements_persist_across_real_batch_boundary() {
    for &n in &[999_usize, 1000, 1001] {
        run_batch_boundary_case(n);
    }
}

/// Hot probes (`check_dedupe`, `path_is_taken`, `existing_seq_for_file`)
/// are about to switch to `prepare_cached` (PERFORMANCE.md #2). The
/// statement cache persists across calls — a regression where bound
/// parameters bleed across queries would surface as either spurious
/// dedupe hits or wrong seq assignments. This test hammers the planner
/// many times against the same state with interleaved file sets and
/// asserts each plan is independently correct.
#[test]
fn many_planner_cycles_share_state_without_bleed() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();

    let day_a = "2024:03:15 09:00:00";
    let day_b = "2024:04:20 09:00:00";

    let names_a = ["a1.jpg", "a2.jpg", "a3.jpg"];
    let names_b = ["b1.jpg", "b2.jpg"];
    for (i, n) in names_a.iter().enumerate() {
        let mut bytes = exif_jpeg(day_a);
        bytes.push(u8::try_from(i + 10).unwrap());
        write_file(&in_dir, n, &bytes, i64::try_from(i + 1).unwrap());
    }
    for (i, n) in names_b.iter().enumerate() {
        let mut bytes = exif_jpeg(day_b);
        bytes.push(u8::try_from(i + 50).unwrap());
        write_file(&in_dir, n, &bytes, i64::try_from(i + 20).unwrap());
    }

    let profile = single_output_profile(&out, "skip", "rename", "output");
    let mut state = State::open_in_memory().unwrap();

    let all_cands = || -> Vec<_> {
        names_a
            .iter()
            .chain(names_b.iter())
            .map(|n| Ok(scanned(&in_dir, n)))
            .collect()
    };

    let p0 = plan(&mut state, &profile, all_cands().into_iter(), None).unwrap();
    shelf::apply::apply(&mut state, &profile, &p0, None, None).unwrap();

    fn count_skips(p: &Plan) -> usize {
        p.actions
            .iter()
            .filter(|a| matches!(a, PlannedAction::SkipDuplicate { .. }))
            .count()
    }
    fn count_places(p: &Plan) -> usize {
        p.actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    PlannedAction::Place { .. } | PlannedAction::Replace { .. }
                )
            })
            .count()
    }

    let expected_total = names_a.len() + names_b.len();
    for i in 0..100 {
        let order: Vec<_> = if i % 2 == 0 {
            all_cands()
        } else {
            let mut v = all_cands();
            v.reverse();
            v
        };
        let p = plan(&mut state, &profile, order.into_iter(), None).unwrap();
        assert_eq!(
            count_skips(&p),
            expected_total,
            "cycle {i}: every prior file should dedupe-skip"
        );
        assert_eq!(count_places(&p), 0, "cycle {i}: no fresh placements");
    }
}

/// Companion to the determinism test: re-running a plan after apply
/// against the same state must produce a stable result. Currently every
/// file is dedupe-skipped on rerun; with the upcoming parallel-hashing
/// change this still must hold. A regression where the second plan
/// emits fresh `Place` actions (because the hash cache misses under
/// the parallel pipeline) would surface here.
#[test]
fn replan_after_apply_is_stable() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();

    write_file(&in_dir, "a.jpg", &exif_jpeg("2024:03:15 09:00:00"), 1);
    write_file(&in_dir, "b.jpg", &exif_jpeg("2024:03:15 10:00:00"), 2);
    write_file(&in_dir, "c.jpg", &exif_jpeg("2024:03:16 09:00:00"), 3);

    let profile = single_output_profile(&out, "skip", "rename", "output");
    let mut state = State::open_in_memory().unwrap();

    let p1 = plan(
        &mut state,
        &profile,
        vec![
            Ok(scanned(&in_dir, "a.jpg")),
            Ok(scanned(&in_dir, "b.jpg")),
            Ok(scanned(&in_dir, "c.jpg")),
        ]
        .into_iter(),
        None,
    )
    .unwrap();
    shelf::apply::apply(&mut state, &profile, &p1, None, None).unwrap();

    let p2 = plan(
        &mut state,
        &profile,
        vec![
            Ok(scanned(&in_dir, "a.jpg")),
            Ok(scanned(&in_dir, "b.jpg")),
            Ok(scanned(&in_dir, "c.jpg")),
        ]
        .into_iter(),
        None,
    )
    .unwrap();

    let places = p2
        .actions
        .iter()
        .filter(|a| {
            matches!(
                a,
                PlannedAction::Place { .. } | PlannedAction::Replace { .. }
            )
        })
        .count();
    let skips = p2
        .actions
        .iter()
        .filter(|a| matches!(a, PlannedAction::SkipDuplicate { .. }))
        .count();
    assert_eq!(places, 0, "rerun must not re-place any file");
    assert_eq!(skips, 3, "rerun must dedupe-skip every prior placement");
}

/// Two files with the same `taken_at` must tie-break on lexicographic
/// `source_path`. The planner sorts `ready` by
/// `(taken_at, source_path)` before seq assignment, so the
/// lex-smaller file always gets the lower seq. Future parallel hashing
/// (PERFORMANCE.md #6) buffers candidates and sorts after — this test
/// pins the tie-break contract so a sort-stability or comparator
/// regression is caught.
#[test]
fn identical_taken_at_tie_breaks_on_source_path_lexicographic() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();

    let same_date = "2024:03:15 12:00:00";
    let mut bytes_a = exif_jpeg(same_date);
    bytes_a.push(b'A');
    let mut bytes_b = exif_jpeg(same_date);
    bytes_b.push(b'B');
    write_file(&in_dir, "alpha.jpg", &bytes_a, 1);
    write_file(&in_dir, "bravo.jpg", &bytes_b, 1);

    let profile = single_output_profile(&out, "skip", "rename", "output");

    let permutations: [Vec<&str>; 4] = [
        vec!["alpha.jpg", "bravo.jpg"],
        vec!["bravo.jpg", "alpha.jpg"],
        vec!["alpha.jpg", "bravo.jpg"],
        vec!["bravo.jpg", "alpha.jpg"],
    ];

    for order in &permutations {
        let mut state = State::open_in_memory().unwrap();
        let cands: Vec<_> = order.iter().map(|n| Ok(scanned(&in_dir, n))).collect();
        let p = plan(&mut state, &profile, cands.into_iter(), None).unwrap();

        let mut by_name: Vec<(String, u64)> = p
            .actions
            .iter()
            .filter_map(|a| match a {
                PlannedAction::Place { src, seq, .. } => Some((
                    src.file_name().unwrap().to_string_lossy().into_owned(),
                    *seq,
                )),
                _ => None,
            })
            .collect();
        by_name.sort();
        let alpha = by_name.iter().find(|(n, _)| n == "alpha.jpg").unwrap().1;
        let bravo = by_name.iter().find(|(n, _)| n == "bravo.jpg").unwrap().1;
        assert!(
            alpha < bravo,
            "alpha.jpg must get the lower seq (got {alpha}) than bravo.jpg ({bravo}) for input order {order:?}"
        );
    }
}

#[test]
fn replanned_file_reuses_prior_seq_via_existing_seq_lookup() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out = tmp.path().join("o");
    fs::create_dir_all(&in_dir).unwrap();

    write_file(&in_dir, "a.jpg", &exif_jpeg("2024:03:15 09:00:00"), 1);
    write_file(&in_dir, "b.jpg", &exif_jpeg("2024:03:15 10:00:00"), 2);
    let profile = single_output_profile(&out, "keep-both", "rename", "output");
    let mut state = State::open_in_memory().unwrap();

    let plan_1 = plan(
        &mut state,
        &profile,
        vec![Ok(scanned(&in_dir, "a.jpg")), Ok(scanned(&in_dir, "b.jpg"))].into_iter(),
        None,
    )
    .unwrap();
    shelf::apply::apply(&mut state, &profile, &plan_1, None, None).unwrap();

    let (seq_a_first, seq_b_first) = {
        let mut by_name: Vec<(String, u64)> = plan_1
            .actions
            .iter()
            .filter_map(|a| match a {
                PlannedAction::Place { src, seq, .. } => Some((
                    src.file_name().unwrap().to_string_lossy().into_owned(),
                    *seq,
                )),
                _ => None,
            })
            .collect();
        by_name.sort();
        let a = by_name.iter().find(|(n, _)| n == "a.jpg").unwrap().1;
        let b = by_name.iter().find(|(n, _)| n == "b.jpg").unwrap().1;
        (a, b)
    };
    assert_ne!(seq_a_first, seq_b_first);

    let plan_2 = plan(
        &mut state,
        &profile,
        vec![Ok(scanned(&in_dir, "a.jpg"))].into_iter(),
        None,
    )
    .unwrap();

    let seq_a_replan = plan_2
        .actions
        .iter()
        .find_map(|a| match a {
            PlannedAction::Place { src, seq, .. } if src.file_name().unwrap() == "a.jpg" => {
                Some(*seq)
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "expected a Place for a.jpg in the second plan, got: {:?}",
                plan_2.actions
            )
        });

    assert_eq!(
        seq_a_replan, seq_a_first,
        "mutated a.jpg must reuse its prior seq via existing_seq_for_file; \
         a broken preload cache would advance to the next free seq"
    );
}

#[test]
fn snapshot_plan_for_small_fixture() {
    let tmp = TempDir::new().unwrap();
    let in_dir = tmp.path().join("in");
    let out_a = tmp.path().join("a");
    let out_b = tmp.path().join("b");
    fs::create_dir_all(&in_dir).unwrap();

    write_file(&in_dir, "first.jpg", &exif_jpeg("2024:03:15 09:00:00"), 1);
    write_file(&in_dir, "second.jpg", &exif_jpeg("2024:03:15 10:00:00"), 2);
    write_file(&in_dir, "third.jpg", &exif_jpeg("2024:03:16 09:00:00"), 3);

    let profile = fanout_profile(&out_a, &out_b);
    let mut state = State::open_in_memory().unwrap();
    let cands = vec![
        Ok(scanned(&in_dir, "first.jpg")),
        Ok(scanned(&in_dir, "second.jpg")),
        Ok(scanned(&in_dir, "third.jpg")),
    ];
    let plan = plan(&mut state, &profile, cands.into_iter(), None).unwrap();

    let mut summary: Vec<String> = plan
        .actions
        .iter()
        .map(|a| describe_action(a, tmp.path()))
        .collect();
    summary.sort();

    insta::assert_debug_snapshot!(summary);
}
