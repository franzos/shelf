//! Profile discovery and name resolution.
//!
//! Resolution rules:
//! - `--config <path>` always wins; the file need not live in the config dir.
//! - A profile name resolves to `<dir>/<name>.toml`; missing → error.
//! - No name + exactly one profile in the dir → use it.
//! - No name + zero/multiple profiles → error.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileEntry {
    /// Filename stem, e.g. `photos` for `photos.toml`.
    pub name: String,
    pub path: PathBuf,
}

/// Resolve the default config directory.
///
/// Order: `$SHELF_CONFIG_DIR` → `$XDG_CONFIG_HOME/shelf` → `$HOME/.config/shelf`.
pub fn default_config_dir() -> Result<PathBuf> {
    if let Ok(val) = std::env::var("SHELF_CONFIG_DIR")
        && !val.is_empty()
    {
        return Ok(PathBuf::from(val));
    }
    if let Ok(val) = std::env::var("XDG_CONFIG_HOME")
        && !val.is_empty()
    {
        return Ok(PathBuf::from(val).join("shelf"));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Ok(PathBuf::from(home).join(".config").join("shelf"));
    }
    Err(Error::NoConfigDir)
}

/// List every `*.toml` directly inside `dir`. Subdirectories are not
/// recursed. Returns an empty vec if the directory does not exist.
pub fn discover_profiles(dir: &Path) -> Result<Vec<ProfileEntry>> {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Io {
                path: dir.to_path_buf(),
                source,
            });
        }
    };

    let mut out = Vec::new();
    for entry in read {
        let entry = entry.map_err(|source| Error::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        let file_type = entry.file_type().map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        if !file_type.is_file() {
            continue;
        }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        out.push(ProfileEntry { name, path });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Resolve a profile invocation to a concrete TOML path.
pub fn resolve_profile(name: Option<&str>, config_override: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = config_override {
        return Ok(path.to_path_buf());
    }

    let dir = default_config_dir()?;

    if let Some(name) = name {
        let path = dir.join(format!("{name}.toml"));
        if path.is_file() {
            Ok(path)
        } else {
            Err(Error::ProfileNotFound {
                name: name.to_string(),
                path,
            })
        }
    } else {
        let entries = discover_profiles(&dir)?;
        match entries.len() {
            0 => Err(Error::NoProfiles { dir }),
            1 => Ok(entries.into_iter().next().unwrap().path),
            n => Err(Error::ProfileAmbiguous {
                dir,
                count: n,
                names: entries.into_iter().map(|e| e.name).collect(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tmpdir")
    }

    #[test]
    fn discovers_toml_files_only() {
        let tmp = make_tmp();
        fs::write(tmp.path().join("photos.toml"), "x = 1").unwrap();
        fs::write(tmp.path().join("docs.toml"), "x = 1").unwrap();
        fs::write(tmp.path().join("README.md"), "no").unwrap();

        let entries = discover_profiles(tmp.path()).unwrap();
        assert_eq!(entries.len(), 2);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["docs", "photos"]);
    }

    #[test]
    fn discovers_missing_dir_is_empty() {
        let tmp = make_tmp();
        let missing = tmp.path().join("does-not-exist");
        let entries = discover_profiles(&missing).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn override_wins() {
        let tmp = make_tmp();
        let p = tmp.path().join("custom.toml");
        fs::write(&p, "x = 1").unwrap();
        let resolved = resolve_profile(Some("ignored"), Some(&p)).unwrap();
        assert_eq!(resolved, p);
    }
}
