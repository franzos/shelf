//! Integration tests for the scanner: walk + filters.

use std::path::{Path, PathBuf};

use assert_fs::TempDir;
use assert_fs::prelude::*;
use shelf::config::Filters;
use shelf::scan::{ScannedFile, scan};

/// Build a `Filters` despite the type being `#[non_exhaustive]`.
fn filters(include: &[&str], exclude: &[&str]) -> Filters {
    let mut f = Filters::default();
    f.include = include.iter().map(|s| (*s).to_string()).collect();
    f.exclude = exclude.iter().map(|s| (*s).to_string()).collect();
    f
}

fn make_tree(root: &Path, files: &[&str]) {
    for rel in files {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("mkdir -p");
        }
        std::fs::write(&p, b"").expect("write empty file");
    }
}

fn collect_rel(inputs: &[PathBuf], filters: &Filters) -> Vec<String> {
    let mut v: Vec<String> = scan(inputs, filters)
        .expect("scan setup")
        .map(|r| {
            r.map(|f| f.relative_path.to_string_lossy().replace('\\', "/"))
                .expect("walk ok")
        })
        .collect();
    v.sort();
    v
}

const TREE: &[&str] = &[
    "a.jpg",
    "b.png",
    "c.gif",
    "sub/d.jpg",
    "sub/e.heic",
    "cache/f.jpg",
    "deep/nested/cache/g.jpg",
    ".thumbnails/h.jpg",
    "notes.txt",
];

fn setup_tree() -> TempDir {
    let tmp = TempDir::new().expect("tmpdir");
    make_tree(tmp.path(), TREE);
    tmp
}

#[test]
fn empty_filters_yields_everything() {
    let tmp = setup_tree();
    let got = collect_rel(&[tmp.path().to_path_buf()], &filters(&[], &[]));
    assert_eq!(got.len(), TREE.len(), "got = {got:#?}");
}

#[test]
fn include_only_yields_matches() {
    let tmp = setup_tree();
    let got = collect_rel(&[tmp.path().to_path_buf()], &filters(&["**/*.jpg"], &[]));
    assert_eq!(
        got,
        vec![
            ".thumbnails/h.jpg",
            "a.jpg",
            "cache/f.jpg",
            "deep/nested/cache/g.jpg",
            "sub/d.jpg",
        ]
    );
}

#[test]
fn exclude_only_subtracts() {
    let tmp = setup_tree();
    let got = collect_rel(
        &[tmp.path().to_path_buf()],
        &filters(&[], &["*.gif", "notes.txt"]),
    );
    assert!(!got.iter().any(|p| p == "c.gif"));
    assert!(!got.iter().any(|p| p == "notes.txt"));
    assert!(got.iter().any(|p| p == "a.jpg"));
}

#[test]
fn include_and_exclude_intersect() {
    let tmp = setup_tree();
    let got = collect_rel(
        &[tmp.path().to_path_buf()],
        &filters(&["**/*.jpg"], &["**/cache/**"]),
    );
    assert_eq!(
        got,
        vec![".thumbnails/h.jpg", "a.jpg", "sub/d.jpg"],
        "got = {got:#?}"
    );
}

#[test]
fn nested_cache_dirs_are_excluded() {
    let tmp = setup_tree();
    let got = collect_rel(&[tmp.path().to_path_buf()], &filters(&[], &["**/cache/**"]));
    assert!(!got.iter().any(|p| p.contains("/cache/")));
    assert!(!got.iter().any(|p| p.starts_with("cache/")));
    assert!(got.iter().any(|p| p == "a.jpg"));
}

#[test]
fn hidden_files_are_scanned_by_default() {
    let tmp = setup_tree();
    let got = collect_rel(&[tmp.path().to_path_buf()], &Filters::default());
    assert!(
        got.iter().any(|p| p == ".thumbnails/h.jpg"),
        "got = {got:#?}"
    );
}

#[test]
fn hidden_files_can_be_excluded_explicitly() {
    let tmp = setup_tree();
    let got = collect_rel(
        &[tmp.path().to_path_buf()],
        &filters(&[], &["**/.thumbnails/**"]),
    );
    assert!(!got.iter().any(|p| p.contains(".thumbnails")));
}

#[test]
fn multiple_inputs_chain_with_source_root() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    make_tree(a.path(), &["one.jpg", "sub/two.jpg"]);
    make_tree(b.path(), &["three.jpg"]);

    let inputs = vec![a.path().to_path_buf(), b.path().to_path_buf()];

    let results: Vec<ScannedFile> = scan(&inputs, &Filters::default())
        .unwrap()
        .map(|r| r.expect("walk ok"))
        .collect();

    assert_eq!(results.len(), 3);

    for f in &results {
        assert!(
            inputs.contains(&f.source_root),
            "unexpected source_root: {:?}",
            f.source_root
        );
        assert!(f.absolute_path.starts_with(&f.source_root));
    }

    assert!(results.iter().any(|f| f.source_root == a.path()));
    assert!(results.iter().any(|f| f.source_root == b.path()));
}

#[test]
fn nonexistent_input_yields_error_item() {
    let missing = PathBuf::from("/definitely/not/here/shelf-test-xyz");
    let items: Vec<_> = scan(std::slice::from_ref(&missing), &Filters::default())
        .unwrap()
        .collect();
    assert_eq!(items.len(), 1, "expected one error item");
    match items.into_iter().next().unwrap() {
        Err(shelf::error::Error::WalkDir { .. }) => {}
        other => panic!("expected WalkDir error, got {other:?}"),
    }
}

#[test]
fn relative_paths_use_forward_slashes() {
    let tmp = TempDir::new().unwrap();
    tmp.child("a/b/c.jpg").touch().unwrap();
    let got: Vec<ScannedFile> = scan(&[tmp.path().to_path_buf()], &Filters::default())
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(got.len(), 1);
    let rel = got[0].relative_path.to_string_lossy().replace('\\', "/");
    assert_eq!(rel, "a/b/c.jpg");
}

#[test]
fn snapshot_representative_fixture() {
    let tmp = setup_tree();
    let got = collect_rel(
        &[tmp.path().to_path_buf()],
        &filters(
            &["**/*.jpg", "**/*.heic"],
            &["**/cache/**", "**/.thumbnails/**"],
        ),
    );
    insta::assert_debug_snapshot!(got);
}

#[test]
fn include_globs_are_case_insensitive() {
    let tmp = TempDir::new().expect("tmpdir");
    make_tree(tmp.path(), &["a.jpg", "B.JPG", "c.Jpeg"]);
    let got = collect_rel(
        &[tmp.path().to_path_buf()],
        &filters(&["*.jpg", "*.jpeg"], &[]),
    );
    assert_eq!(got, vec!["B.JPG", "a.jpg", "c.Jpeg"], "got = {got:#?}");
}

#[test]
fn exclude_globs_are_case_insensitive() {
    let tmp = TempDir::new().expect("tmpdir");
    make_tree(tmp.path(), &["keep.jpg", "drop.gif", "also.GIF"]);
    let got = collect_rel(&[tmp.path().to_path_buf()], &filters(&[], &["*.GIF"]));
    assert_eq!(got, vec!["keep.jpg"], "got = {got:#?}");
}

/// Non-UTF-8 paths must round-trip through the scanner without panicking.
/// Glob matching operates on raw `OsStr` bytes per the module-level note,
/// so a byte-level extension filter still matches. Pins the contract
/// before PERFORMANCE.md #4 switches `path_to_string` to `Cow<str>`: a
/// regression that bails on non-UTF-8 instead of falling back to lossy
/// encoding would surface here.
#[cfg(unix)]
#[test]
fn non_utf8_filename_is_scanned_not_panicked_on() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let tmp = TempDir::new().expect("tmpdir");
    let mut name_bytes: Vec<u8> = b"weird-".to_vec();
    name_bytes.extend_from_slice(&[0xFF, 0xFE]);
    name_bytes.extend_from_slice(b".jpg");
    let weird = OsStr::from_bytes(&name_bytes);
    let weird_path = tmp.path().join(weird);
    std::fs::write(&weird_path, b"").expect("write non-utf8 named file");

    std::fs::write(tmp.path().join("normal.jpg"), b"").unwrap();

    let items: Vec<ScannedFile> = scan(&[tmp.path().to_path_buf()], &filters(&["*.jpg"], &[]))
        .unwrap()
        .map(|r| r.expect("walk ok"))
        .collect();
    assert_eq!(
        items.len(),
        2,
        "both the normal and non-UTF-8 named .jpg must be yielded; got {items:?}"
    );

    let any_weird = items
        .iter()
        .any(|f| f.absolute_path.file_name() == Some(weird));
    assert!(
        any_weird,
        "the non-UTF-8 path must appear with its original bytes intact, got {items:?}"
    );
}
