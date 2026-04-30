//! SHA-256 snapshot/verify utilities for `plan.md`.
//!
//! Used by the runner to detect agent tampering: the runner snapshots
//! `plan.md` before dispatching the implementer, then [`verify_unchanged`]
//! after. Any difference — including whitespace-only edits — produces a
//! [`SnapshotError::Mismatch`] so the run can halt and restore from the
//! pre-agent backup.

use std::fmt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

/// SHA-256 hash of a file's bytes captured at a point in time.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    hash: [u8; 32],
}

impl Snapshot {
    /// Hash the bytes directly. Useful in tests and when the contents are
    /// already in memory.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Snapshot {
            hash: hasher.finalize().into(),
        }
    }

    /// Hex digest, lowercase. Stable for logging.
    pub fn hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in &self.hash {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Snapshot").field(&self.hex()).finish()
    }
}

/// Errors produced by [`snapshot`] and [`verify_unchanged`].
#[derive(Debug, Error)]
pub enum SnapshotError {
    /// Reading the file failed.
    #[error("snapshot: failed to read {path:?}: {source}")]
    Io {
        /// File whose read failed.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The current file contents don't hash to the expected value.
    #[error("snapshot: contents of {path:?} have changed since the snapshot was taken")]
    Mismatch {
        /// File whose contents drifted.
        path: PathBuf,
    },
}

/// Compute a [`Snapshot`] of the file at `path`.
pub fn snapshot(path: impl AsRef<Path>) -> Result<Snapshot, SnapshotError> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).map_err(|source| SnapshotError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Snapshot::of_bytes(&bytes))
}

/// Verify that the file at `path` still hashes to `expected`. Returns
/// [`SnapshotError::Mismatch`] on any drift, including whitespace-only changes.
pub fn verify_unchanged(
    path: impl AsRef<Path>,
    expected: &Snapshot,
) -> Result<(), SnapshotError> {
    let path = path.as_ref();
    let current = snapshot(path)?;
    if current != *expected {
        return Err(SnapshotError::Mismatch {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn detects_byte_for_byte_equality() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("plan.md");
        fs::write(&path, b"hello").unwrap();
        let snap = snapshot(&path).unwrap();
        verify_unchanged(&path, &snap).unwrap();
    }

    #[test]
    fn detects_whitespace_only_changes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("plan.md");
        fs::write(&path, b"hello\n").unwrap();
        let snap = snapshot(&path).unwrap();
        // Add a single trailing space — content drifted.
        fs::write(&path, b"hello \n").unwrap();
        let err = verify_unchanged(&path, &snap).unwrap_err();
        assert!(matches!(err, SnapshotError::Mismatch { .. }));
    }

    #[test]
    fn detects_content_changes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("plan.md");
        fs::write(&path, b"original").unwrap();
        let snap = snapshot(&path).unwrap();
        fs::write(&path, b"tampered").unwrap();
        let err = verify_unchanged(&path, &snap).unwrap_err();
        assert!(matches!(err, SnapshotError::Mismatch { .. }));
    }

    #[test]
    fn missing_file_is_io_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.md");
        let err = snapshot(&path).unwrap_err();
        assert!(matches!(err, SnapshotError::Io { .. }));
    }

    #[test]
    fn hex_is_lowercase_64_chars() {
        let s = Snapshot::of_bytes(b"abc");
        let h = s.hex();
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn snapshots_round_trip_via_bytes() {
        let a = Snapshot::of_bytes(b"hello");
        let b = Snapshot::of_bytes(b"hello");
        assert_eq!(a, b);
        let c = Snapshot::of_bytes(b"helloX");
        assert_ne!(a, c);
    }
}
