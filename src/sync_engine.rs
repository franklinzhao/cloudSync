use anyhow::Result;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncReadExt;
use tracing::{debug, info, warn};

use crate::cloud::{CloudProvider, RemoteFile};
use crate::state_db::{FileRecord, StateDb, SyncStatus};

/// The core reconciliation logic.
/// Compares local and remote state and performs uploads / downloads as needed.
pub struct SyncEngine<'a> {
    db: &'a StateDb,
    provider: &'a dyn CloudProvider,
    local_root: &'a Path,
    exclude_patterns: Vec<glob::Pattern>,
    max_parallel_downloads: usize,
    max_parallel_uploads: usize,
}

impl<'a> SyncEngine<'a> {
    pub fn new(
        db: &'a StateDb,
        provider: &'a dyn CloudProvider,
        local_root: &'a Path,
        exclude_patterns: &[String],
        max_parallel_downloads: usize,
        max_parallel_uploads: usize,
    ) -> Self {
        let patterns = exclude_patterns
            .iter()
            .filter_map(|p| glob::Pattern::new(p).ok())
            .collect();
        SyncEngine {
            db,
            provider,
            local_root,
            exclude_patterns: patterns,
            max_parallel_downloads: max_parallel_downloads.max(1),
            max_parallel_uploads: max_parallel_uploads.max(1),
        }
    }

    /// Check whether a path matches any exclude pattern or is known system metadata
    fn is_excluded(&self, relative: &str) -> bool {
        let path = Path::new(relative);

        // Always exclude macOS / OS-level metadata anywhere in the directory hierarchy
        for comp in path.components() {
            if let std::path::Component::Normal(os_str) = comp {
                let name = os_str.to_string_lossy();
                if name == ".DS_Store"
                    || name == "Thumbs.db"
                    || name == ".git"
                    || name == ".Trash"
                    || name.starts_with("._")
                {
                    return true;
                }
            }
        }

        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        self.exclude_patterns.iter().any(|p| {
            p.matches(relative) || (!file_name.is_empty() && p.matches(file_name))
        })
    }

    /// Check if a path component contains characters forbidden by OneDrive
    pub fn find_invalid_char(relative: &str) -> Option<char> {
        const FORBIDDEN: &[char] = &[':', '*', '?', '"', '<', '>', '|', '\\'];
        for comp in Path::new(relative).components() {
            if let std::path::Component::Normal(os_str) = comp {
                let s = os_str.to_string_lossy();
                if let Some(c) = s.chars().find(|c| FORBIDDEN.contains(c)) {
                    return Some(c);
                }
            }
        }
        None
    }

    /// Run a full two-way sync reconciliation pass
    pub async fn reconcile(&self) -> Result<SyncSummary> {
        info!("Starting sync reconciliation…");
        let mut summary = SyncSummary::default();

        // 1. Fetch remote file list and queue downloads / handle deletions
        self.scan_remote(&mut summary).await?;

        // 2. Walk the local folder and queue any new/changed files for upload
        self.scan_local(&mut summary).await?;

        info!(
            "Reconciliation done — uploaded: {}, downloaded: {}, skipped: {}, errors: {}",
            summary.uploaded, summary.downloaded, summary.skipped, summary.errors
        );
        Ok(summary)
    }

    /// Sync a single changed file path (called from the FS watcher)
    pub async fn sync_path(&self, local_path: &Path) -> Result<()> {
        if !local_path.starts_with(self.local_root) {
            warn!("Path outside sync root, ignoring: {}", local_path.display());
            return Ok(());
        }

        let relative = local_path
            .strip_prefix(self.local_root)?
            .to_str()
            .unwrap_or("")
            .to_string();

        if self.is_excluded(&relative) {
            debug!("Excluded: {}", relative);
            return Ok(());
        }

        if !local_path.exists() {
            self.handle_local_delete(&relative).await?;
            return Ok(());
        }

        if local_path.is_dir() {
            return Ok(());
        }

        if is_online_only(local_path) {
            debug!("Ignoring online-only placeholder event: {}", relative);
            return Ok(());
        }

        if let Some(bad_char) = Self::find_invalid_char(&relative) {
            warn!(
                "⚠️ Skipping upload for '{}': OneDrive forbids character '{}'. Rename the file on your Mac to sync it.",
                relative, bad_char
            );
            return Ok(());
        }

        let local_hash = hash_file(local_path).await?;
        let record = self.db.get(&relative)?;

        // If the local file hash matches the recorded state in StateDb,
        // it was either downloaded from the cloud or touched without content modification.
        // Skip uploading to avoid echo loops.
        if let Some(ref r) = record {
            if r.local_hash.as_deref() == Some(&local_hash) {
                debug!("Local file content unchanged for {}, skipping upload", relative);
                return Ok(());
            }
        }

        self.upload_file(local_path, &relative).await?;
        Ok(())
    }

    // ── Private helpers ────────────────────────────────────────────────────

    async fn scan_local(&self, summary: &mut SyncSummary) -> Result<()> {
        let entries = walk_dir(self.local_root).await?;
        let mut to_upload: Vec<(PathBuf, String)> = Vec::new();

        for local_path in entries {
            let relative = local_path
                .strip_prefix(self.local_root)?
                .to_str()
                .unwrap_or("")
                .to_string();

            if self.is_excluded(&relative) {
                continue;
            }

            if is_online_only(&local_path) {
                debug!("Skipping online-only placeholder in scan_local: {}", relative);
                continue;
            }

            if let Some(bad_char) = Self::find_invalid_char(&relative) {
                warn!(
                    "⚠️ Skipping upload for '{}': OneDrive forbids character '{}'. Rename the file on your Mac to sync it.",
                    relative, bad_char
                );
                summary.errors += 1;
                continue;
            }

            let local_hash = hash_file(&local_path).await?;
            let record = self.db.get(&relative)?;

            let needs_upload = match &record {
                None => true, // new file
                Some(r) => r.local_hash.as_deref() != Some(&local_hash), // changed
            };

            if needs_upload {
                to_upload.push((local_path, relative));
            } else {
                summary.skipped += 1;
            }
        }

        if !to_upload.is_empty() {
            info!(
                "Uploading {} files (up to {} parallel)…",
                to_upload.len(),
                self.max_parallel_uploads
            );

            let provider = self.provider;
            let db = self.db;

            let mut stream = futures::stream::iter(to_upload.into_iter().map(|(local_path, relative)| {
                async move {
                    let rel = relative.clone();
                    let res = Self::upload_and_record_static(provider, db, &local_path, &relative).await;
                    (rel, res)
                }
            }))
            .buffer_unordered(self.max_parallel_uploads);

            while let Some((rel, res)) = stream.next().await {
                match res {
                    Ok(_) => summary.uploaded += 1,
                    Err(e) => {
                        warn!("Upload failed for {}: {}", rel, e);
                        summary.errors += 1;
                    }
                }
            }
        }

        Ok(())
    }

    async fn scan_remote(&self, summary: &mut SyncSummary) -> Result<()> {
        let remote_files = self.provider.list_files().await?;
        let remote_paths: std::collections::HashSet<String> = remote_files
            .iter()
            .filter(|r| !r.is_dir)
            .map(|r| r.relative_path.clone())
            .collect();

        let mut remote_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in &remote_files {
            if r.is_dir {
                remote_dirs.insert(r.relative_path.clone());
            }
            let mut p = Path::new(&r.relative_path).parent();
            while let Some(parent) = p {
                let s = parent.to_string_lossy();
                if !s.is_empty() {
                    remote_dirs.insert(s.to_string());
                }
                p = parent.parent();
            }
        }

        let mut to_download: Vec<(RemoteFile, PathBuf)> = Vec::new();

        for remote in &remote_files {
            if remote.is_dir {
                if !self.is_excluded(&remote.relative_path) {
                    let local_path = self.local_root.join(&remote.relative_path);
                    if !local_path.exists() {
                        let _ = fs::create_dir_all(&local_path).await;
                    }
                }
                continue;
            }
            if self.is_excluded(&remote.relative_path) { continue; }

            let local_path = self.local_root.join(&remote.relative_path);
            let is_placeholder = local_path.exists() && is_online_only(&local_path);

            if is_placeholder {
                info!("Detected online-only placeholder, queueing download: {}", remote.relative_path);
                to_download.push((remote.clone(), local_path));
            } else if !local_path.exists() {
                // Check if this file was renamed on the cloud (same remote_id in DB with a different path)
                let renamed_from = self.db.find_by_remote_id(&remote.id)?;
                let mut handled_rename = false;

                if let Some(old_record) = renamed_from {
                    if old_record.relative_path != remote.relative_path {
                        let old_local_path = self.local_root.join(&old_record.relative_path);
                        if old_local_path.exists() {
                            if let Some(parent) = local_path.parent() {
                                fs::create_dir_all(parent).await?;
                            }
                            info!("Remote rename detected: {} → {}", old_record.relative_path, remote.relative_path);
                            match fs::rename(&old_local_path, &local_path).await {
                                Ok(_) => {
                                    self.db.remove(&old_record.relative_path)?;

                                    // If the remote file was also modified (not just renamed), download updated content
                                    let content_changed = old_record.remote_etag.as_deref() != Some(&remote.etag);
                                    if content_changed {
                                        to_download.push((remote.clone(), local_path.clone()));
                                    } else {
                                        let hash = hash_file(&local_path).await.unwrap_or_default();
                                        let local_modified = fs::metadata(&local_path)
                                            .await
                                            .ok()
                                            .and_then(|m| m.modified().ok())
                                            .map(chrono::DateTime::<chrono::Utc>::from);
                                        self.db.upsert(&FileRecord {
                                            relative_path:      remote.relative_path.clone(),
                                            local_hash:         Some(hash),
                                            remote_etag:        Some(remote.etag.clone()),
                                            remote_id:          Some(remote.id.clone()),
                                            remote_modified_at: Some(remote.modified_at),
                                            local_modified_at:  local_modified,
                                            status:             SyncStatus::Synced,
                                        })?;
                                        summary.downloaded += 1;
                                    }

                                    handled_rename = true;
                                }
                                Err(e) => {
                                    warn!("Failed to rename local file ({}), will download: {}", e, remote.relative_path);
                                }
                            }
                        }
                    }
                }

                if !handled_rename {
                    // Check if this file was previously synced on this machine, but deleted locally while offline
                    let existing_record = self.db.get(&remote.relative_path)?;
                    if let Some(ref record) = existing_record {
                        let remote_etag_matches = record.remote_etag.as_deref() == Some(&remote.etag);
                        if remote_etag_matches {
                            // Remote file is unchanged, but local copy was removed while offline
                            info!("Local deletion detected (offline), deleting from remote: {}", remote.relative_path);
                            if let Err(e) = self.provider.delete(&remote.id).await {
                                warn!("Failed to delete remote file {}: {}", remote.relative_path, e);
                            }
                            self.db.remove(&remote.relative_path)?;
                            continue;
                        } else {
                            info!("Remote modified while deleted locally, queueing download: {}", remote.relative_path);
                        }
                    }

                    // File exists remotely but not locally — queue download
                    to_download.push((remote.clone(), local_path));
                }
            } else {
                // File exists both sides — check if remote is newer (last-write-wins)
                let record = self.db.get(&remote.relative_path)?;
                let remote_etag_changed = match &record {
                    Some(r) => r.remote_etag.as_deref() != Some(&remote.etag),
                    None => true,
                };

                if remote_etag_changed {
                    let local_modified = fs::metadata(&local_path)
                        .await
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .map(chrono::DateTime::<chrono::Utc>::from);

                    let remote_newer = local_modified
                        .map(|lm| remote.modified_at > lm)
                        .unwrap_or(true);

                    if remote_newer {
                        to_download.push((remote.clone(), local_path));
                    }
                    // else: local is newer — it will have been picked up by scan_local
                }
            }
        }

        // ── Execute downloads concurrently ──────────────────────────────────
        if !to_download.is_empty() {
            info!(
                "Downloading {} files (up to {} parallel)…",
                to_download.len(),
                self.max_parallel_downloads
            );

            let provider = self.provider;
            let db = self.db;

            let mut stream = futures::stream::iter(to_download.into_iter().map(|(remote, local_path)| {
                async move {
                    let rel = remote.relative_path.clone();
                    info!("Downloading: {}", rel);
                    let res = Self::download_and_record_static(provider, db, &remote, &local_path).await;
                    (rel, res)
                }
            }))
            .buffer_unordered(self.max_parallel_downloads);

            while let Some((rel, res)) = stream.next().await {
                match res {
                    Ok(_) => summary.downloaded += 1,
                    Err(e) => {
                        warn!("Download failed for {}: {}", rel, e);
                        summary.errors += 1;
                    }
                }
            }
        }

        // ── Clean up files and folders deleted on the remote ────────────────
        // Any file tracked in StateDb that is no longer on the remote was deleted
        // (or renamed) on the cloud.
        let mut candidate_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut deleted_file_ancestors: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

        for record in self.db.all_records()? {
            if self.is_excluded(&record.relative_path) {
                continue;
            }

            if !remote_paths.contains(&record.relative_path) {
                let local_path = self.local_root.join(&record.relative_path);
                if local_path.exists() {
                    let is_placeholder = is_online_only(&local_path);
                    let current_local_hash = if is_placeholder {
                        String::new()
                    } else {
                        hash_file(&local_path).await.unwrap_or_default()
                    };

                    if is_placeholder || record.local_hash.as_deref() == Some(&current_local_hash) {
                        info!("Remote deletion detected, removing local: {}", record.relative_path);
                        if let Err(e) = fs::remove_file(&local_path).await {
                            warn!("Failed to remove local file {}: {}", record.relative_path, e);
                        } else {
                            // Collect ancestor directories up to local_root for cleanup
                            let mut curr = local_path.parent();
                            while let Some(parent) = curr {
                                if parent == self.local_root || !parent.starts_with(self.local_root) {
                                    break;
                                }
                                candidate_dirs.insert(parent.to_path_buf());
                                deleted_file_ancestors.insert(parent.to_path_buf());
                                curr = parent.parent();
                            }
                        }
                    } else {
                        warn!(
                            "Conflict: {} was deleted remotely but modified locally. Preserving local copy.",
                            record.relative_path
                        );
                    }
                }
                self.db.remove(&record.relative_path)?;
            }
        }

        // Also check any empty local directories that no longer exist on remote
        let local_dirs = collect_local_dirs(self.local_root).await;
        for d in local_dirs {
            candidate_dirs.insert(d);
        }

        // Clean up empty directories from deepest to shallowest
        let mut sorted_candidates: Vec<PathBuf> = candidate_dirs.into_iter().collect();
        sorted_candidates.sort_by_key(|a| std::cmp::Reverse(a.components().count()));

        for dir in sorted_candidates {
            if !dir.exists() || !dir.is_dir() {
                continue;
            }

            let relative = match dir.strip_prefix(self.local_root) {
                Ok(p) => p.to_string_lossy().to_string(),
                Err(_) => continue,
            };

            if relative.is_empty() || self.is_excluded(&relative) {
                continue;
            }

            // If the directory still exists on the remote, preserve it
            if remote_dirs.contains(&relative) {
                continue;
            }

            // Check if directory is empty or only contains disposable OS metadata (e.g. .DS_Store)
            if is_dir_empty_or_only_metadata(&dir).await.unwrap_or(false) {
                // If the directory was not an ancestor of a file deleted in this run,
                // and it was created very recently (< 10s), give the user a moment
                // (e.g. they just clicked "New Folder" in Finder).
                if !deleted_file_ancestors.contains(&dir) {
                    let is_very_recent = if let Ok(meta) = fs::metadata(&dir).await {
                        meta.modified().ok()
                            .and_then(|m| m.elapsed().ok())
                            .map(|elapsed| elapsed < std::time::Duration::from_secs(10))
                            .unwrap_or(false)
                    } else {
                        false
                    };

                    if is_very_recent {
                        continue;
                    }
                }

                info!("Remote folder deletion detected, removing local empty directory: {}", relative);
                if let Err(e) = remove_dir_and_disposable_metadata(&dir).await {
                    warn!("Failed to remove local empty directory {}: {}", relative, e);
                }
            }
        }

        Ok(())
    }

    async fn download_and_record_static(
        provider: &dyn CloudProvider,
        db: &StateDb,
        remote: &RemoteFile,
        local_path: &Path,
    ) -> Result<()> {
        provider.download(&remote.id, local_path).await?;
        let hash = hash_file(local_path).await.unwrap_or_default();
        let local_modified = fs::metadata(local_path)
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .map(chrono::DateTime::<chrono::Utc>::from);
        db.upsert(&FileRecord {
            relative_path:      remote.relative_path.clone(),
            local_hash:         Some(hash),
            remote_etag:        Some(remote.etag.clone()),
            remote_id:          Some(remote.id.clone()),
            remote_modified_at: Some(remote.modified_at),
            local_modified_at:  local_modified,
            status:             SyncStatus::Synced,
        })?;
        Ok(())
    }

    async fn upload_and_record_static(
        provider: &dyn CloudProvider,
        db: &StateDb,
        local_path: &Path,
        relative: &str,
    ) -> Result<()> {
        info!("Uploading: {}", relative);
        let remote_file = provider.upload(local_path, relative).await?;
        let local_hash = hash_file(local_path).await?;

        let meta = fs::metadata(local_path).await?;
        let local_modified = meta.modified().ok()
            .map(chrono::DateTime::<chrono::Utc>::from);

        db.upsert(&FileRecord {
            relative_path:      relative.to_string(),
            local_hash:         Some(local_hash),
            remote_etag:        Some(remote_file.etag),
            remote_id:          Some(remote_file.id),
            remote_modified_at: Some(remote_file.modified_at),
            local_modified_at:  local_modified,
            status:             SyncStatus::Synced,
        })?;

        Ok(())
    }

    async fn upload_file(&self, local_path: &Path, relative: &str) -> Result<()> {
        Self::upload_and_record_static(self.provider, self.db, local_path, relative).await
    }

    async fn handle_local_delete(&self, relative: &str) -> Result<()> {
        info!("Local delete detected: {}", relative);
        if let Some(record) = self.db.get(relative)? {
            if let Some(remote_id) = record.remote_id {
                if let Err(e) = self.provider.delete(&remote_id).await {
                    warn!("Failed to delete remote item {}: {}", relative, e);
                }
            }
            self.db.remove(relative)?;
        }

        // Also check if relative was a folder containing tracked files
        let prefix = format!("{}/", relative.trim_end_matches('/'));
        for record in self.db.all_records()? {
            if record.relative_path.starts_with(&prefix) {
                info!("Local folder delete: deleting remote file {}", record.relative_path);
                if let Some(remote_id) = record.remote_id {
                    if let Err(e) = self.provider.delete(&remote_id).await {
                        warn!("Failed to delete remote file {}: {}", record.relative_path, e);
                    }
                }
                self.db.remove(&record.relative_path)?;
            }
        }

        Ok(())
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

/// Check whether a file name is disposable OS metadata (safe to delete when cleaning up empty dirs)
fn is_disposable_metadata(name: &str) -> bool {
    name == ".DS_Store"
        || name == "Thumbs.db"
        || name == "desktop.ini"
        || name.starts_with("._")
}

/// Check if a directory is either completely empty or contains only disposable OS metadata
async fn is_dir_empty_or_only_metadata(dir: &Path) -> Result<bool> {
    let mut entries = match fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return Ok(false),
    };

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            return Ok(false);
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !is_disposable_metadata(&name_str) {
            return Ok(false);
        }
    }

    Ok(true)
}

/// Remove disposable OS metadata files from a directory and then remove the directory itself
async fn remove_dir_and_disposable_metadata(dir: &Path) -> Result<()> {
    let mut entries = fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_file() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if is_disposable_metadata(&name_str) {
                let _ = fs::remove_file(&path).await;
            }
        }
    }
    fs::remove_dir(dir).await?;
    Ok(())
}

/// Recursively collect all directory paths under a root
async fn collect_local_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        if let Ok(mut entries) = fs::read_dir(&dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_dir() {
                    let name = entry.file_name();
                    let s = name.to_string_lossy();
                    if s == ".git" || s == ".Trash" {
                        continue;
                    }
                    dirs.push(path.clone());
                    stack.push(path);
                }
            }
        }
    }
    dirs
}

/// Recursively collect all file paths (not dirs) under a root
async fn walk_dir(root: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut result = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let mut entries = fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name();
                let s = name.to_string_lossy();
                if s == ".git" || s == ".Trash" {
                    continue;
                }
                stack.push(path);
            } else {
                result.push(path);
            }
        }
    }

    Ok(result)
}

/// Check whether a file on disk is an online-only placeholder (dataless file).
/// On macOS (APFS / FileProvider / OneDrive), dataless files have the SF_DATALESS
/// flag set (0x4000_0000) or report len > 0 while allocating 0 filesystem blocks.
/// Reading such files triggers on-demand faulting which hangs if the provider daemon
/// is unresponsive.
pub fn is_online_only(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt as MacExt;
        use std::os::unix::fs::MetadataExt as UnixExt;

        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if meta.is_file() {
                // SF_DATALESS flag is 0x4000_0000 in <sys/stat.h> on macOS.
                let dataless_flag: u32 = 0x4000_0000;
                if (meta.st_flags() & dataless_flag) != 0 {
                    return true;
                }
                if meta.len() > 0 && meta.blocks() == 0 {
                    return true;
                }
            }
        }
        false
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            let attrs = meta.file_attributes();
            let recall_on_access: u32 = 0x0040_0000;
            let offline: u32 = 0x0000_1000;
            if (attrs & recall_on_access) != 0 || (attrs & offline) != 0 {
                return true;
            }
        }
        false
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = path;
        false
    }
}

/// Compute the SHA-256 hex digest of a local file
pub async fn hash_file(path: &Path) -> Result<String> {
    if is_online_only(path) {
        return Err(anyhow::anyhow!(
            "Cannot hash online-only dataless placeholder: {}",
            path.display()
        ));
    }
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Summary of a sync pass
#[derive(Debug, Default)]
pub struct SyncSummary {
    pub uploaded:   usize,
    pub downloaded: usize,
    pub skipped:    usize,
    pub errors:     usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_normal_file_is_not_online_only() {
        let dir = std::env::temp_dir().join(format!("cloudsync_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("normal.txt");
        let mut f = std::fs::File::create(&file_path).unwrap();
        f.write_all(b"hello world").unwrap();
        f.flush().unwrap();

        assert!(!is_online_only(&file_path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_hash_normal_file() {
        let dir = std::env::temp_dir().join(format!("cloudsync_test_hash_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("hello.txt");
        let mut f = std::fs::File::create(&file_path).unwrap();
        f.write_all(b"hello world").unwrap();
        f.flush().unwrap();

        let hash = hash_file(&file_path).await.unwrap();
        // sha256("hello world") = b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9
        assert_eq!(hash, "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_detect_onedrive_pictures_online_only() {
        let pictures_dir = Path::new("/Users/franklinzhao/Library/CloudStorage/OneDrive-Personal/SheridanFiles/Pictures");
        if pictures_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(pictures_dir) {
                let mut total_files = 0;
                let mut online_only_count = 0;
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() {
                        total_files += 1;
                        if is_online_only(&path) {
                            online_only_count += 1;
                        }
                    }
                }
                println!(
                    "Pictures folder scan: {}/{} files identified as online-only",
                    online_only_count, total_files
                );
            }
        }
    }

    struct DummyProvider;
    #[async_trait::async_trait]
    impl CloudProvider for DummyProvider {
        fn name(&self) -> &str { "dummy" }
        async fn list_files(&self) -> Result<Vec<crate::cloud::RemoteFile>> { Ok(vec![]) }
        async fn upload(&self, _local_path: &Path, _remote_relative_path: &str) -> Result<crate::cloud::RemoteFile> {
            unimplemented!()
        }
        async fn download(&self, _remote_id: &str, _local_path: &Path) -> Result<()> {
            unimplemented!()
        }
        async fn delete(&self, _remote_id: &str) -> Result<()> {
            unimplemented!()
        }
        async fn create_dir(&self, _remote_relative_path: &str) -> Result<Option<String>> {
            unimplemented!()
        }
    }

    #[test]
    fn test_ds_store_and_exclusion() {
        let dummy_provider = DummyProvider;
        let db_path = std::env::temp_dir().join(format!("test_db_{}.db", std::process::id()));
        let db = StateDb::open(&db_path).unwrap();
        let engine = SyncEngine::new(
            &db,
            &dummy_provider,
            Path::new("/dummy/local"),
            &[".DS_Store".to_string(), "*.tmp".to_string()],
            4,
            2,
        );

        // Top level and subfolder .DS_Store
        assert!(engine.is_excluded(".DS_Store"));
        assert!(engine.is_excluded("Pictures/.DS_Store"));
        assert!(engine.is_excluded("a/b/c/.DS_Store"));

        // Windows Thumbs.db & .git
        assert!(engine.is_excluded("Thumbs.db"));
        assert!(engine.is_excluded("sub/Thumbs.db"));
        assert!(engine.is_excluded(".git/config"));
        assert!(engine.is_excluded("project/.git/HEAD"));

        // Glob wildcard pattern *.tmp at any depth
        assert!(engine.is_excluded("temp.tmp"));
        assert!(engine.is_excluded("sub/folder/data.tmp"));

        // Valid file should NOT be excluded
        assert!(!engine.is_excluded("photo.jpg"));
        assert!(!engine.is_excluded("sub/folder/report.pdf"));

        // Invalid OneDrive characters check
        assert_eq!(SyncEngine::find_invalid_char("report:2025.pdf"), Some(':'));
        assert_eq!(SyncEngine::find_invalid_char("folder/https:www.creditkarma.ca:report.pdf"), Some(':'));
        assert_eq!(SyncEngine::find_invalid_char("what?file.txt"), Some('?'));
        assert_eq!(SyncEngine::find_invalid_char("normal_file.pdf"), None);

        let _ = std::fs::remove_file(&db_path);
    }

    struct MockProvider {
        files: Vec<RemoteFile>,
    }
    #[async_trait::async_trait]
    impl CloudProvider for MockProvider {
        fn name(&self) -> &str { "mock" }
        async fn list_files(&self) -> Result<Vec<RemoteFile>> {
            Ok(self.files.clone())
        }
        async fn upload(&self, _local_path: &Path, _remote_relative_path: &str) -> Result<RemoteFile> {
            unimplemented!()
        }
        async fn download(&self, _remote_id: &str, _local_path: &Path) -> Result<()> {
            unimplemented!()
        }
        async fn delete(&self, _remote_id: &str) -> Result<()> {
            Ok(())
        }
        async fn create_dir(&self, _remote_relative_path: &str) -> Result<Option<String>> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn test_remote_folder_deletion_cleans_empty_dir() {
        let temp_dir = std::env::temp_dir().join(format!("cloudsync_test_del_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = std::fs::create_dir_all(&temp_dir);

        let sub_dir = temp_dir.join("MyFolder").join("Nested");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let file_path = sub_dir.join("test.txt");
        let ds_store_path = sub_dir.join(".DS_Store");
        std::fs::write(&file_path, b"content").unwrap();
        std::fs::write(&ds_store_path, b"fake ds_store").unwrap();

        let db_path = temp_dir.join("test.db");
        let db = StateDb::open(&db_path).unwrap();

        // Record file in DB as previously synced
        let hash = hash_file(&file_path).await.unwrap();
        db.upsert(&FileRecord {
            relative_path: "MyFolder/Nested/test.txt".to_string(),
            local_hash: Some(hash),
            remote_etag: Some("etag123".to_string()),
            remote_id: Some("id123".to_string()),
            remote_modified_at: Some(chrono::Utc::now()),
            local_modified_at: None,
            status: SyncStatus::Synced,
        }).unwrap();

        // Cloud provider now has NO files (the folder was deleted on cloud)
        let provider = MockProvider { files: vec![] };
        let engine = SyncEngine::new(
            &db,
            &provider,
            &temp_dir,
            &[".DS_Store".to_string()],
            1,
            1,
        );

        let mut summary = SyncSummary::default();
        engine.scan_remote(&mut summary).await.unwrap();

        // Both file and nested folders should be gone
        assert!(!file_path.exists(), "File should be deleted");
        assert!(!sub_dir.exists(), "Nested directory should be deleted");
        assert!(!temp_dir.join("MyFolder").exists(), "MyFolder directory should be deleted");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_remote_folder_deletion_preserves_dir_with_remaining_files() {
        let temp_dir = std::env::temp_dir().join(format!("cloudsync_test_keep_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = std::fs::create_dir_all(&temp_dir);

        let my_dir = temp_dir.join("MyFolder");
        std::fs::create_dir_all(&my_dir).unwrap();
        let file1 = my_dir.join("deleted.txt");
        let file2 = my_dir.join("keep.txt");
        std::fs::write(&file1, b"deleted content").unwrap();
        std::fs::write(&file2, b"keep content").unwrap();

        let db_path = temp_dir.join("test.db");
        let db = StateDb::open(&db_path).unwrap();

        let hash1 = hash_file(&file1).await.unwrap();
        db.upsert(&FileRecord {
            relative_path: "MyFolder/deleted.txt".to_string(),
            local_hash: Some(hash1),
            remote_etag: Some("etag1".to_string()),
            remote_id: Some("id1".to_string()),
            remote_modified_at: Some(chrono::Utc::now()),
            local_modified_at: None,
            status: SyncStatus::Synced,
        }).unwrap();

        // Cloud provider has no files
        let provider = MockProvider { files: vec![] };
        let engine = SyncEngine::new(
            &db,
            &provider,
            &temp_dir,
            &[],
            1,
            1,
        );

        let mut summary = SyncSummary::default();
        engine.scan_remote(&mut summary).await.unwrap();

        // file1 should be deleted, but file2 and MyFolder should remain
        assert!(!file1.exists(), "Deleted file should be removed");
        assert!(file2.exists(), "Remaining file must be preserved");
        assert!(my_dir.exists(), "MyFolder must be preserved because it still contains file2");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}

