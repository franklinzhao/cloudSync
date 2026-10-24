use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::path::Path;

pub mod onedrive;

/// Metadata about a remote file
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RemoteFile {
    /// Unique identifier on the cloud provider
    pub id: String,
    /// File name (not full path)
    pub name: String,
    /// Full remote path relative to the sync root folder
    pub relative_path: String,
    /// Provider-specific version identifier (eTag, revision, etc.)
    pub etag: String,
    /// Last modification time on the remote
    pub modified_at: DateTime<Utc>,
    /// File size in bytes
    pub size: u64,
    /// Whether this entry is a directory
    pub is_dir: bool,
}

/// Unified interface for cloud storage providers
#[async_trait]
#[allow(dead_code)]
pub trait CloudProvider: Send + Sync {
    /// Return the name of this provider (e.g. "OneDrive")
    fn name(&self) -> &str;

    /// List all files recursively under the remote sync folder
    async fn list_files(&self) -> Result<Vec<RemoteFile>>;

    /// Upload a local file to the given remote path (relative to sync root)
    /// Returns the new remote file metadata on success
    async fn upload(
        &self,
        local_path: &Path,
        remote_relative_path: &str,
    ) -> Result<RemoteFile>;

    /// Download a remote file (by ID) to the given local path
    async fn download(
        &self,
        remote_id: &str,
        local_path: &Path,
    ) -> Result<()>;

    /// Delete a remote file by ID
    async fn delete(&self, remote_id: &str) -> Result<()>;

    /// Create a remote directory (no-op for providers that don't need explicit dir creation)
    async fn create_dir(&self, remote_relative_path: &str) -> Result<Option<String>>;
}
