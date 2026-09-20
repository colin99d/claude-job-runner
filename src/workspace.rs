//! Throw-away working directories, one per job.
//!
//! Every job gets a fresh directory under a dedicated root. Claude Code
//! runs with that directory as its `cwd`, which is the only place the
//! sandbox lets it write. When the job is over the directory is deleted,
//! taking the session's scratch files with it.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::fs;
use tracing::{info, warn};

use crate::job::MessageId;

/// Prefix of every directory the runner creates under the root, so the
/// start-up sweep never touches anything it did not make itself.
const DIR_PREFIX: &str = "job-";

/// Errors from workspace management.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// Creating or removing a directory failed.
    #[error("{action} {}", path.display())]
    Io {
        /// What we were doing.
        action: &'static str,
        /// Which path.
        path: PathBuf,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
}

/// The directory that holds all per-job workspaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    /// Ensures the root exists and returns an absolute handle to it.
    pub async fn prepare(path: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let path = path.as_ref();
        fs::create_dir_all(path)
            .await
            .map_err(|source| WorkspaceError::Io {
                action: "creating workspace root",
                path: path.to_path_buf(),
                source,
            })?;
        let absolute = fs::canonicalize(path)
            .await
            .map_err(|source| WorkspaceError::Io {
                action: "resolving workspace root",
                path: path.to_path_buf(),
                source,
            })?;
        Ok(Self(absolute))
    }

    /// Absolute path of the root.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Deletes leftover workspaces from previous runs.
    ///
    /// Only directories named `job-*` are touched. Returns how many were removed.
    pub async fn sweep(&self) -> Result<usize, WorkspaceError> {
        let mut entries = fs::read_dir(&self.0)
            .await
            .map_err(|source| WorkspaceError::Io {
                action: "listing workspace root",
                path: self.0.clone(),
                source,
            })?;
        let mut removed = 0;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| WorkspaceError::Io {
                action: "listing workspace root",
                path: self.0.clone(),
                source,
            })?
        {
            let is_ours = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(DIR_PREFIX));
            let is_dir = entry.file_type().await.is_ok_and(|t| t.is_dir());
            if !(is_ours && is_dir) {
                continue;
            }
            let path = entry.path();
            match fs::remove_dir_all(&path).await {
                Ok(()) => {
                    info!(path = %path.display(), "removed stale workspace");
                    removed += 1;
                }
                Err(err) => {
                    warn!(path = %path.display(), error = %err, "could not remove stale workspace");
                }
            }
        }
        Ok(removed)
    }
}

/// A single job's directory. Call [`Workspace::remove`] when done.
#[derive(Debug)]
pub struct Workspace {
    path: PathBuf,
}

impl Workspace {
    /// Creates a new empty directory for `job` under `root`.
    pub async fn create(root: &WorkspaceRoot, job: MessageId) -> Result<Self, WorkspaceError> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = root.path().join(format!("{DIR_PREFIX}{job}-{nanos}"));
        fs::create_dir(&path)
            .await
            .map_err(|source| WorkspaceError::Io {
                action: "creating workspace",
                path: path.clone(),
                source,
            })?;
        Ok(Self { path })
    }

    /// Absolute path of the directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Deletes the directory and everything in it.
    pub async fn remove(self) -> Result<(), WorkspaceError> {
        fs::remove_dir_all(&self.path)
            .await
            .map_err(|source| WorkspaceError::Io {
                action: "removing workspace",
                path: self.path.clone(),
                source,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_remove_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = WorkspaceRoot::prepare(tmp.path().join("ws")).await.unwrap();
        assert!(root.path().is_absolute());

        let ws = Workspace::create(&root, MessageId::new(42)).await.unwrap();
        assert!(ws.path().is_dir());
        assert!(ws.path().starts_with(root.path()));
        assert!(
            ws.path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("job-42-")
        );

        let path = ws.path().to_path_buf();
        tokio::fs::write(path.join("scratch.txt"), b"x")
            .await
            .unwrap();
        ws.remove().await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn sweep_only_removes_job_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let root = WorkspaceRoot::prepare(tmp.path()).await.unwrap();

        let stale = Workspace::create(&root, MessageId::new(1)).await.unwrap();
        let stale_path = stale.path().to_path_buf();
        std::mem::forget(stale);
        tokio::fs::create_dir(root.path().join("keep-me"))
            .await
            .unwrap();
        tokio::fs::write(root.path().join("job-notes.txt"), b"x")
            .await
            .unwrap();

        assert_eq!(root.sweep().await.unwrap(), 1);
        assert!(!stale_path.exists());
        assert!(root.path().join("keep-me").is_dir());
        assert!(root.path().join("job-notes.txt").is_file());
    }
}
