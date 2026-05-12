//! Streaming sha256 over file contents.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// 64 KiB matches the page size of most filesystems and keeps peak RSS
/// negligible without sacrificing throughput.
const READ_BUF: usize = 64 * 1024;

/// Stream `path` through sha256, returning the 32-byte digest.
pub fn sha256_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path).map_err(|source| Error::Hash {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; READ_BUF];
    loop {
        let n = file.read(&mut buf).map_err(|source| Error::Hash {
            path: path.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// Lowercase hex encoding of a 32-byte digest.
#[must_use]
pub fn hex(hash: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for byte in hash {
        use std::fmt::Write as _;
        let _ = write!(s, "{byte:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn empty_file_hashes_to_known_value() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let h = sha256_file(tmp.path()).unwrap();
        assert_eq!(
            hex(&h),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn identical_bytes_hash_identically() {
        let mut a = tempfile::NamedTempFile::new().unwrap();
        let mut b = tempfile::NamedTempFile::new().unwrap();
        a.write_all(b"hello world").unwrap();
        b.write_all(b"hello world").unwrap();
        a.flush().unwrap();
        b.flush().unwrap();
        assert_eq!(
            sha256_file(a.path()).unwrap(),
            sha256_file(b.path()).unwrap()
        );
    }

    #[test]
    fn different_bytes_hash_differently() {
        let mut a = tempfile::NamedTempFile::new().unwrap();
        let mut b = tempfile::NamedTempFile::new().unwrap();
        a.write_all(b"hello world").unwrap();
        b.write_all(b"hello worle").unwrap();
        a.flush().unwrap();
        b.flush().unwrap();
        assert_ne!(
            sha256_file(a.path()).unwrap(),
            sha256_file(b.path()).unwrap()
        );
    }

    #[test]
    fn known_vector() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"abc").unwrap();
        f.flush().unwrap();
        assert_eq!(
            hex(&sha256_file(f.path()).unwrap()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );
    }

    #[test]
    fn streams_files_larger_than_buf() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let data = vec![0x5au8; 256 * 1024];
        f.write_all(&data).unwrap();
        f.flush().unwrap();
        let h = sha256_file(f.path()).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let expected: [u8; 32] = hasher.finalize().into();
        assert_eq!(h, expected);
    }

    #[test]
    fn missing_file_yields_hash_error() {
        let err = sha256_file(Path::new("/nonexistent/shelf/xyz/none")).unwrap_err();
        match err {
            Error::Hash { path, .. } => {
                assert_eq!(path, Path::new("/nonexistent/shelf/xyz/none"));
            }
            other => panic!("expected Hash error, got {other:?}"),
        }
    }
}
