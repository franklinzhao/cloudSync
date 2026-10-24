use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;
use futures::StreamExt;
use reqwest::{header, Client, StatusCode};
use serde::Deserialize;
use std::path::Path;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::OneDriveConfig;
use super::{CloudProvider, RemoteFile};

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
/// Maximum chunk size for large-file upload sessions (must be multiple of 320 KiB)
const UPLOAD_CHUNK_SIZE: usize = 10 * 320 * 1024; // ~3.2 MB

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Percent-encode each path segment but keep the '/' separators.
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|seg| urlencoding::encode(seg).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

// ── Graph API response types ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DriveItem {
    id: String,
    name: String,
    #[serde(rename = "eTag")]
    e_tag: Option<String>,
    #[serde(rename = "lastModifiedDateTime")]
    last_modified_date_time: Option<String>,
    size: Option<u64>,
    folder: Option<serde_json::Value>,
    #[serde(rename = "parentReference")]
    parent_reference: Option<ParentRef>,
}

#[derive(Debug, Deserialize)]
struct ParentRef {
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DriveItemList {
    value: Vec<DriveItem>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UploadSession {
    #[serde(rename = "uploadUrl")]
    upload_url: String,
}

// ── OneDrive client ──────────────────────────────────────────────────────────

/// OneDrive cloud provider backed by the Microsoft Graph API
pub struct OneDriveClient {
    http: Client,
    /// Thread-safe access token (auto-refreshes if expired)
    access_token: Arc<RwLock<String>>,
    /// Root folder path on OneDrive, e.g. "/Documents/Sync"
    remote_root: String,
    /// OAuth config for auto-refreshing expired tokens
    oauth_config: Option<OneDriveConfig>,
}

impl OneDriveClient {
    pub fn new(
        access_token: String,
        remote_root: String,
        oauth_config: Option<OneDriveConfig>,
    ) -> Self {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("Failed to build HTTP client");
        OneDriveClient {
            http,
            access_token: Arc::new(RwLock::new(access_token)),
            remote_root,
            oauth_config,
        }
    }

    /// Clone client sharing the same HTTP connection pool and access token, but targeting a different remote root
    pub fn with_remote_root(&self, remote_root: String) -> Self {
        OneDriveClient {
            http: self.http.clone(),
            access_token: Arc::clone(&self.access_token),
            remote_root,
            oauth_config: self.oauth_config.clone(),
        }
    }

    /// Update the access token (called after a refresh)
    pub async fn set_token(&self, token: String) {
        let mut w = self.access_token.write().await;
        *w = token;
    }

    /// Authorization header value
    async fn auth_header(&self) -> String {
        let r = self.access_token.read().await;
        format!("Bearer {}", *r)
    }

    /// Attempt to refresh token if 401 is received
    async fn refresh_token_if_needed(&self) -> Result<String> {
        if let Some(ref cfg) = self.oauth_config {
            info!("Access token expired or rejected (401), refreshing token…");
            let new_token = crate::auth::get_access_token(cfg, &self.http).await?;
            let mut w = self.access_token.write().await;
            *w = new_token.clone();
            info!("Access token refreshed successfully.");
            Ok(new_token)
        } else {
            Err(anyhow!("No OAuth configuration available to refresh token"))
        }
    }

    /// Execute a Graph API request with retry for:
    /// - 429 Too Many Requests (honors Retry-After header)
    /// - 503 Service Unavailable (exponential backoff)
    /// - 401 Unauthorized (auto-refreshes token once)
    async fn request_with_retry<F>(&self, make_req: F) -> Result<reqwest::Response>
    where
        F: Fn(&str) -> reqwest::RequestBuilder,
    {
        let mut attempt = 0u32;
        let mut refreshed_token = false;

        loop {
            let auth = self.auth_header().await;
            let req = make_req(&auth);
            let resp = req.send().await?;

            let status = resp.status();

            // 1. Handle Token Expiration (401 Unauthorized)
            if status == StatusCode::UNAUTHORIZED && !refreshed_token && self.oauth_config.is_some() {
                refreshed_token = true;
                if let Err(e) = self.refresh_token_if_needed().await {
                    warn!("Token auto-refresh failed: {}", e);
                    return Ok(resp);
                }
                continue;
            }

            // 2. Handle Rate Limiting (429) and Temporary Server Errors (503)
            if (status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE) && attempt < 6 {
                let wait = resp
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or_else(|| (2u64.pow(attempt)).min(60));

                warn!(
                    "Microsoft Graph throttled ({status}), retrying in {wait}s (attempt {}/6)",
                    attempt + 1
                );
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                attempt += 1;
                continue;
            }

            return Ok(resp);
        }
    }

    /// Remote root normalised to "/name" with no trailing slash.
    /// Returns "" when the root is the drive root itself.
    fn root_path(&self) -> String {
        let trimmed = self.remote_root.trim_matches('/');
        if trimmed.is_empty() {
            String::new()
        } else {
            format!("/{}", trimmed)
        }
    }

    /// Graph URL addressing an item by path, e.g. ".../me/drive/root:/a/b:".
    /// An empty path addresses the drive root itself.
    fn item_url(&self, full_path: &str) -> String {
        if full_path.is_empty() {
            format!("{}/me/drive/root", GRAPH_BASE)
        } else {
            format!("{}/me/drive/root:{}:", GRAPH_BASE, encode_path(full_path))
        }
    }

    /// Convert a Graph item to our RemoteFile, building the relative path from
    /// the parent reference and item name.
    fn to_remote_file(&self, item: &DriveItem, base_prefix: &str) -> RemoteFile {
        let parent_path = item
            .parent_reference
            .as_ref()
            .and_then(|p| p.path.as_deref())
            .unwrap_or("");

        // Graph returns e.g. "/drive/root:/Sync/sub%20dir" (percent-encoded)
        let after_colon = parent_path.split_once(':').map(|(_, p)| p).unwrap_or("");
        let decoded = urlencoding::decode(after_colon)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| after_colon.to_string());

        let relative_parent = decoded
            .strip_prefix(base_prefix)
            .unwrap_or(&decoded)
            .trim_matches('/');

        let relative_path = if relative_parent.is_empty() {
            item.name.clone()
        } else {
            format!("{}/{}", relative_parent, item.name)
        };

        RemoteFile {
            id:            item.id.clone(),
            name:          item.name.clone(),
            relative_path,
            etag:          item.e_tag.clone().unwrap_or_default(),
            modified_at:   item.last_modified_date_time
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(Utc::now),
            size:          item.size.unwrap_or(0),
            is_dir:        item.folder.is_some(),
        }
    }
}

#[async_trait]
impl CloudProvider for OneDriveClient {
    fn name(&self) -> &str { "OneDrive" }

    async fn list_files(&self) -> Result<Vec<RemoteFile>> {
        let mut all_items: Vec<RemoteFile> = Vec::new();
        let base_prefix = self.root_path();

        // No $expand: Graph rejects multi-level expansion. We walk folders ourselves.
        let mut page_url: Option<String> = Some(format!(
            "{}/children?$top=200",
            self.item_url(&base_prefix)
        ));
        let mut folder_queue: Vec<String> = Vec::new();
        let mut is_root_request = true;

        loop {
            // Next page of the current folder, otherwise the next queued subfolder
            let url = match page_url.take().or_else(|| {
                folder_queue.pop().map(|id| {
                    format!("{}/me/drive/items/{}/children?$top=200", GRAPH_BASE, id)
                })
            }) {
                Some(u) => u,
                None => break,
            };

            let resp = self.request_with_retry(|auth| {
                self.http
                    .get(&url)
                    .header(header::AUTHORIZATION, auth)
            }).await?;

            if resp.status() == StatusCode::NOT_FOUND {
                if is_root_request {
                    info!("Remote folder '{}' does not exist yet", base_prefix);
                    return Ok(all_items);
                }
                warn!("A remote folder disappeared during listing, skipping it");
                continue;
            }
            is_root_request = false;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(anyhow!("Graph list failed ({status}): {body}"));
            }

            let list: DriveItemList = resp.json().await?;
            for item in &list.value {
                let rf = self.to_remote_file(item, &base_prefix);
                if rf.is_dir {
                    folder_queue.push(item.id.clone());
                }
                all_items.push(rf);
            }
            page_url = list.next_link;
        }

        Ok(all_items)
    }

    async fn upload(
        &self,
        local_path: &Path,
        remote_relative_path: &str,
    ) -> Result<RemoteFile> {
        let metadata = fs::metadata(local_path).await?;
        let file_size = metadata.len() as usize;

        if file_size <= 4 * 1024 * 1024 {
            // Small file — direct PUT upload
            self.upload_small(local_path, remote_relative_path).await
        } else {
            // Large file — resumable upload session
            self.upload_large(local_path, remote_relative_path, file_size).await
        }
    }

    async fn download(&self, remote_id: &str, local_path: &Path) -> Result<()> {
        // Get the download URL from Graph with retry
        let meta_url = format!("{}/me/drive/items/{}", GRAPH_BASE, remote_id);
        let resp = self.request_with_retry(|auth| {
            self.http
                .get(&meta_url)
                .header(header::AUTHORIZATION, auth)
                .query(&[("$select", "@microsoft.graph.downloadUrl")])
        }).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Graph item fetch failed ({status}): {body}"));
        }

        #[derive(Deserialize)]
        struct DownloadMeta {
            #[serde(rename = "@microsoft.graph.downloadUrl")]
            download_url: Option<String>,
        }
        let meta: DownloadMeta = resp.json().await?;
        let url = meta.download_url
            .ok_or_else(|| anyhow!("No download URL for item {}", remote_id))?;

        // Stream the download to the local file
        if let Some(parent) = local_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        // Write to a temporary file first in the same directory, then atomically replace.
        // Uses PID + nanosecond timestamp to ensure concurrent downloads never collide.
        let rand_suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let file_name = local_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("download");
        let tmp_file_name = format!(".{file_name}.cloudsync_{}_{}.tmp", std::process::id(), rand_suffix);
        let tmp_path = local_path.with_file_name(tmp_file_name);

        let mut file = fs::File::create(&tmp_path).await?;
        let res = async {
            // Initiate stream from CDN (Azure Blob Storage) with retry for transient throttling/network hiccups
            let mut stream_resp = None;
            for attempt in 0..4 {
                let res = self.http.get(&url).send().await;
                match res {
                    Ok(r) if r.status().is_success() => {
                        stream_resp = Some(r);
                        break;
                    }
                    Ok(r) if r.status() == StatusCode::TOO_MANY_REQUESTS || r.status() == StatusCode::SERVICE_UNAVAILABLE => {
                        let wait = r.headers()
                            .get(header::RETRY_AFTER)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or_else(|| (2u64.pow(attempt)).min(30));
                        warn!("CDN download stream throttled ({}), retrying in {}s...", r.status(), wait);
                        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                    }
                    Ok(r) => {
                        let status = r.status();
                        let body = r.text().await.unwrap_or_default();
                        return Err(anyhow!("CDN download failed ({status}): {body}"));
                    }
                    Err(e) if attempt < 3 => {
                        warn!("CDN connection dropped ({e}), retrying stream...");
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            let stream_resp = stream_resp.ok_or_else(|| anyhow!("Failed to initiate download stream after retries"))?;
            let mut stream = stream_resp.bytes_stream();

            while let Some(chunk) = stream.next().await {
                file.write_all(&chunk?).await?;
            }
            file.flush().await?;
            Ok::<(), anyhow::Error>(())
        }.await;

        drop(file);

        if let Err(e) = res {
            let _ = fs::remove_file(&tmp_path).await;
            return Err(e);
        }

        // Remove placeholder or existing file before rename if it exists
        if local_path.exists() {
            let _ = fs::remove_file(local_path).await;
        }
        if let Err(e) = fs::rename(&tmp_path, local_path).await {
            let _ = fs::remove_file(&tmp_path).await;
            return Err(e.into());
        }

        info!("Downloaded {} → {}", remote_id, local_path.display());
        Ok(())
    }

    async fn delete(&self, remote_id: &str) -> Result<()> {
        let url = format!("{}/me/drive/items/{}", GRAPH_BASE, remote_id);
        let resp = self.request_with_retry(|auth| {
            self.http
                .delete(&url)
                .header(header::AUTHORIZATION, auth)
        }).await?;

        match resp.status() {
            StatusCode::NO_CONTENT | StatusCode::OK => {
                info!("Deleted remote item {}", remote_id);
                Ok(())
            }
            StatusCode::NOT_FOUND => {
                warn!("Remote item {} already gone, skipping delete.", remote_id);
                Ok(())
            }
            s => {
                let body = resp.text().await.unwrap_or_default();
                Err(anyhow!("Delete failed ({s}): {body}"))
            }
        }
    }

    async fn create_dir(&self, remote_relative_path: &str) -> Result<Option<String>> {
        let (dir_name, parent_path) = match remote_relative_path.rsplit_once('/') {
            Some((parent, name)) => (name, format!("{}/{}", self.root_path(), parent)),
            None => (remote_relative_path, self.root_path()),
        };

        let url = format!("{}/children", self.item_url(&parent_path));
        let body = serde_json::json!({
            "name": dir_name,
            "folder": {},
            "@microsoft.graph.conflictBehavior": "fail"
        });

        let resp = self.request_with_retry(|auth| {
            self.http
                .post(&url)
                .header(header::AUTHORIZATION, auth)
                .json(&body)
        }).await?;

        if resp.status().is_success() {
            let item: DriveItem = resp.json().await?;
            return Ok(Some(item.id));
        }

        // Folder already exists: return its id instead of creating a duplicate
        if resp.status() == StatusCode::CONFLICT {
            let full = format!("{}/{}", self.root_path(), remote_relative_path);
            let existing = self.request_with_retry(|auth| {
                self.http
                    .get(self.item_url(&full))
                    .header(header::AUTHORIZATION, auth)
            }).await?;
            if existing.status().is_success() {
                let item: DriveItem = existing.json().await?;
                return Ok(Some(item.id));
            }
        }

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(anyhow!("Create dir failed ({status}): {body}"))
    }
}

// ── Upload helpers ────────────────────────────────────────────────────────────

impl OneDriveClient {
    async fn upload_small(
        &self,
        local_path: &Path,
        remote_relative_path: &str,
    ) -> Result<RemoteFile> {
        let data = fs::read(local_path).await?;
        let remote_full = format!("{}/{}", self.root_path(), remote_relative_path);
        let url = format!("{}/content", self.item_url(&remote_full));

        debug!("Uploading (small) {} → {}", local_path.display(), remote_full);

        let data_ref = &data;
        let resp = self.request_with_retry(|auth| {
            self.http
                .put(&url)
                .header(header::AUTHORIZATION, auth)
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(data_ref.clone())
        }).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Upload failed ({status}): {body}"));
        }

        let item: DriveItem = resp.json().await?;
        let base_prefix = self.root_path();
        Ok(self.to_remote_file(&item, &base_prefix))
    }

    async fn upload_large(
        &self,
        local_path: &Path,
        remote_relative_path: &str,
        file_size: usize,
    ) -> Result<RemoteFile> {
        // 1. Create an upload session with retry
        let remote_full = format!("{}/{}", self.root_path(), remote_relative_path);
        let session_url = format!("{}/createUploadSession", self.item_url(&remote_full));

        let session_body = serde_json::json!({
            "item": {
                "@microsoft.graph.conflictBehavior": "replace"
            }
        });

        let resp = self.request_with_retry(|auth| {
            self.http
                .post(&session_url)
                .header(header::AUTHORIZATION, auth)
                .header(header::CONTENT_TYPE, "application/json")
                .json(&session_body)
        }).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Create upload session failed ({status}): {body}"));
        }

        let session: UploadSession = resp.json().await?;
        let data = fs::read(local_path).await?;

        // 2. Upload in chunks (chunks must stay sequential for a single file)
        let mut offset = 0usize;
        let mut last_item: Option<DriveItem> = None;

        while offset < file_size {
            let end = (offset + UPLOAD_CHUNK_SIZE).min(file_size);
            let chunk = &data[offset..end];

            let range = format!("bytes {}-{}/{}", offset, end - 1, file_size);
            debug!("Uploading chunk {} ({} bytes)", range, chunk.len());

            let mut chunk_resp = None;
            for attempt in 0..6 {
                let resp = self.http
                    .put(&session.upload_url)
                    .header("Content-Range", &range)
                    .header(header::CONTENT_LENGTH, chunk.len().to_string())
                    .body(chunk.to_vec())
                    .send()
                    .await?;

                let s = resp.status();
                if s == StatusCode::TOO_MANY_REQUESTS || s == StatusCode::SERVICE_UNAVAILABLE {
                    let wait = resp
                        .headers()
                        .get(header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or_else(|| (2u64.pow(attempt)).min(60));
                    warn!("Chunk upload throttled ({s}), retrying in {wait}s (attempt {}/6)", attempt + 1);
                    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                    continue;
                }

                chunk_resp = Some(resp);
                break;
            }

            let resp = chunk_resp.ok_or_else(|| anyhow!("Chunk upload failed after retries for {}", remote_relative_path))?;

            match resp.status() {
                StatusCode::ACCEPTED => {
                    // More chunks needed
                    offset = end;
                }
                StatusCode::CREATED | StatusCode::OK => {
                    // Upload complete
                    last_item = Some(resp.json().await?);
                    break;
                }
                s => {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(anyhow!("Chunk upload failed ({s}): {body}"));
                }
            }
        }

        let item = last_item.ok_or_else(|| anyhow!("Large upload did not return final item"))?;
        let base_prefix = self.root_path();
        Ok(self.to_remote_file(&item, &base_prefix))
    }
}