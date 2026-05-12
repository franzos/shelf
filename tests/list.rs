//! Integration tests for `shelf list` and profile resolution at the CLI seam.

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use std::fs;

fn bin() -> Command {
    Command::cargo_bin("shelf").expect("binary `shelf` not found")
}

const MINIMAL_PROFILE: &str = r#"
inputs = ["/tmp/in"]

[[output]]
name = "lib"
path = "/tmp/out"
directory = "{yyyy}/{mm}"
filename  = "{yyyy}-{mm}-{dd}_{seq:05}"
"#;

#[test]
fn list_enumerates_profiles_in_config_dir() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    fs::write(tmp.path().join("photos.toml"), MINIMAL_PROFILE).unwrap();
    fs::write(tmp.path().join("invoices.toml"), MINIMAL_PROFILE).unwrap();
    // Files that aren't `*.toml` should be ignored.
    fs::write(tmp.path().join("README.md"), "ignore me").unwrap();

    bin()
        .env("SHELF_CONFIG_DIR", tmp.path())
        .arg("list")
        .assert()
        .success()
        .stdout(contains("photos").and(contains("invoices")))
        .stdout(contains("README").not());
}

#[test]
fn list_with_empty_config_dir_succeeds_silently() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    bin()
        .env("SHELF_CONFIG_DIR", tmp.path())
        .arg("list")
        .assert()
        .success()
        .stdout("");
}

#[test]
fn verify_subcommand_succeeds_on_empty_library() {
    // M13 wired up `shelf verify`. On a freshly-created profile with no
    // placements, verify is a clean no-op (exit 0) — see `tests/verify.rs`
    // for the destination-side drift coverage.
    let tmp = tempfile::tempdir().expect("tmpdir");
    fs::write(tmp.path().join("photos.toml"), MINIMAL_PROFILE).unwrap();
    bin()
        .env("SHELF_CONFIG_DIR", tmp.path())
        .args(["verify", "photos"])
        .assert()
        .success();
}
