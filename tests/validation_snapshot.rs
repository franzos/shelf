//! Snapshot test for the human-facing validation error format.

use std::fs;

use shelf::config::load_profile;
use shelf::error::Error;

/// Two distinct problems so we exercise the multi-error pretty-printer:
/// missing `inputs` is caught at serde time so it would never reach the
/// validator — instead we trigger a duplicate output name plus a bad glob.
const BROKEN_PROFILE: &str = r#"
inputs = ["/tmp/in"]

[filters]
include = ["*["]

[kinds]
photo = ["jpg", "png"]
raw   = ["jpg", "cr3"]

[[output]]
name = "lib"
path = "/tmp/a"
directory = "{yyyy}"
filename  = "{yyyy}-{mm}"

[[output]]
name = "lib"
path = "/tmp/b"
directory = "{yyyy"
filename  = "{}"
"#;

#[test]
fn validation_error_format_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("broken.toml");
    fs::write(&path, BROKEN_PROFILE).unwrap();

    let err = load_profile(&path).expect_err("expected validation failure");
    let msg = match err {
        Error::Validation { errors, .. } => {
            // Render without the unstable absolute tmpdir path.
            let mut s = String::from("profile `<tmp>/broken.toml` failed validation:\n");
            for (i, e) in errors.iter().enumerate() {
                if i > 0 {
                    s.push('\n');
                }
                s.push_str("  - ");
                s.push_str(&e.to_string());
            }
            s
        }
        other => panic!("expected Error::Validation, got {other:?}"),
    };

    insta::assert_snapshot!(msg);
}
