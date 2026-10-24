use anyhow::Result;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

/// Individual folder pair entry in config.toml.
/// Can be defined as a `subfolder` relative to `local_parent_folder`
/// or as a direct `local_folder` path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FolderPairItem {
    /// Relative subfolder under local_parent_folder (e.g. "SheridanFiles/Pictures")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subfolder: Option<String>,

    /// Full local folder path (optional if subfolder is specified)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_folder: Option<PathBuf>,

    /// Remote folder path on OneDrive (e.g. "/SheridanFiles/Pictures")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_folder: Option<String>,

    /// Whether this folder pair is actively syncing
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl FolderPairItem {
    /// Resolve this entry against an optional parent folder into a fully-qualified `FolderPair`
    pub fn resolve(&self, parent: Option<&Path>) -> FolderPair {
        let (local_path, rel_str) = if let Some(sub) = &self.subfolder {
            let clean_sub = sub.trim_matches('/');
            let full_path = if let Some(p) = parent {
                p.join(clean_sub)
            } else {
                PathBuf::from(sub)
            };
            (full_path, clean_sub.to_string())
        } else if let Some(loc) = &self.local_folder {
            if loc.is_relative() {
                if let Some(p) = parent {
                    (p.join(loc), loc.to_string_lossy().to_string())
                } else {
                    (loc.clone(), loc.to_string_lossy().to_string())
                }
            } else {
                (loc.clone(), loc.to_string_lossy().to_string())
            }
        } else {
            (PathBuf::from("."), String::new())
        };

        let remote_path = if let Some(rem) = &self.remote_folder {
            if !rem.starts_with('/') {
                format!("/{}", rem)
            } else {
                rem.clone()
            }
        } else {
            format!("/{}", rel_str.trim_matches('/'))
        };

        FolderPair {
            local_folder: local_path,
            remote_folder: remote_path,
            enabled: self.enabled,
        }
    }
}

/// Resolved active folder synchronization pair used by the sync engine and watcher
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FolderPair {
    /// Absolute local folder path to sync
    pub local_folder: PathBuf,
    /// Remote folder path on OneDrive (e.g. "/Documents/Sync")
    pub remote_folder: String,
    /// Whether this folder pair is actively syncing
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Application-wide configuration, loaded from config.toml
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Shared parent local folder for all sync folders (avoids repeating long paths)
    #[serde(default, alias = "local_base_folder", alias = "parent_folder", skip_serializing_if = "Option::is_none")]
    pub local_parent_folder: Option<PathBuf>,

    /// List of sync folder pairs (bi-directional sync with dataless hydration)
    #[serde(default)]
    pub sync_folders: Vec<FolderPairItem>,

    /// Legacy single local folder (for backward compatibility)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_folder: Option<PathBuf>,

    /// Legacy single remote folder (for backward compatibility)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_folder: Option<String>,

    /// How often (in seconds) to do a full reconcile pass even without FS events
    #[serde(default = "default_sync_interval")]
    pub sync_interval_secs: u64,

    /// File/directory glob patterns to exclude from sync
    #[serde(default)]
    pub exclude_patterns: Vec<String>,

    /// OneDrive OAuth2 credentials (registered app)
    pub onedrive: OneDriveConfig,

    /// Log verbosity: "trace", "debug", "info", "warn", "error"
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Maximum concurrent file downloads (default: 4)
    #[serde(default = "default_max_parallel_downloads")]
    pub max_parallel_downloads: usize,

    /// Maximum concurrent file uploads (default: 2)
    #[serde(default = "default_max_parallel_uploads")]
    pub max_parallel_uploads: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OneDriveConfig {
    /// Azure AD application (client) ID
    pub client_id: String,

    /// OAuth2 redirect URI (must match Azure app registration)
    #[serde(default = "default_redirect_uri")]
    pub redirect_uri: String,
}

fn default_sync_interval() -> u64 { 300 } // 5 minutes
fn default_log_level() -> String { "info".to_string() }
fn default_redirect_uri() -> String { "http://localhost:8765/callback".to_string() }
fn default_max_parallel_downloads() -> usize { 4 }
fn default_max_parallel_uploads() -> usize { 2 }

impl Config {
    /// Return all configured sync folder pairs with resolved absolute paths.
    /// Uses `local_parent_folder` to resolve subfolders if specified.
    pub fn get_sync_folders(&self) -> Vec<FolderPair> {
        let parent = self.local_parent_folder.as_deref();
        if !self.sync_folders.is_empty() {
            return self
                .sync_folders
                .iter()
                .map(|item| item.resolve(parent))
                .collect();
        }
        if let (Some(l), Some(r)) = (&self.local_folder, &self.remote_folder) {
            let full_local = if l.is_relative() {
                if let Some(p) = parent {
                    p.join(l)
                } else {
                    l.clone()
                }
            } else {
                l.clone()
            };
            vec![FolderPair {
                local_folder: full_local,
                remote_folder: r.clone(),
                enabled: true,
            }]
        } else {
            Vec::new()
        }
    }

    /// Update the sync folder pairs list, stripping parent prefix when local_parent_folder is set
    #[allow(dead_code)]
    pub fn set_sync_folders(&mut self, folders: Vec<FolderPair>) {
        let parent = self.local_parent_folder.as_ref();
        let mut items = Vec::new();

        for f in &folders {
            let (subfolder, local_folder) = if let Some(p) = parent {
                if let Ok(rel) = f.local_folder.strip_prefix(p) {
                    (Some(rel.to_string_lossy().to_string()), None)
                } else {
                    (None, Some(f.local_folder.clone()))
                }
            } else {
                (None, Some(f.local_folder.clone()))
            };

            items.push(FolderPairItem {
                subfolder,
                local_folder,
                remote_folder: Some(f.remote_folder.clone()),
                enabled: f.enabled,
            });
        }

        if let Some(first) = folders.first() {
            self.local_folder = Some(first.local_folder.clone());
            self.remote_folder = Some(first.remote_folder.clone());
        }
        self.sync_folders = items;
    }

    /// Path to the config file: ~/Library/Application Support/cloudsync/config.toml
    pub fn config_path() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("cloudsync")
            .join("config.toml")
    }

    /// Load config from disk, returning an error if the file doesn't exist or is malformed
    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        let content = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("Cannot read config at {}: {}", path.display(), e))?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    /// Save the config to disk
    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        tracing::info!("Saved configuration to {}", path.display());
        Ok(())
    }

    /// Write an example config file if none exists
    pub fn write_example_if_missing() -> Result<()> {
        let path = Self::config_path();
        if path.exists() {
            return Ok(());
        }
        let example = include_str!("../config.toml.example");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, example)?;
        tracing::info!("Wrote example config to {}", path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_legacy_config_deserialization() {
        let toml_str = r#"
            local_folder = "/Users/test/Documents"
            remote_folder = "/Documents"
            sync_interval_secs = 120

            [onedrive]
            client_id = "test-client-id"
        "#;

        let cfg: Config = toml::from_str(toml_str).expect("Failed to parse legacy config");
        assert_eq!(cfg.sync_interval_secs, 120);
        assert_eq!(cfg.max_parallel_downloads, 4);
        assert_eq!(cfg.max_parallel_uploads, 2);

        let folders = cfg.get_sync_folders();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].local_folder, PathBuf::from("/Users/test/Documents"));
        assert_eq!(folders[0].remote_folder, "/Documents");
        assert!(folders[0].enabled);
    }

    #[test]
    fn test_multi_folder_deserialization() {
        let toml_str = r#"
            sync_interval_secs = 60
            max_parallel_downloads = 8
            max_parallel_uploads = 3

            [[sync_folders]]
            local_folder = "/Users/test/Pictures"
            remote_folder = "/Pictures"
            enabled = true

            [[sync_folders]]
            local_folder = "/Users/test/Notes"
            remote_folder = "/Notes"
            enabled = false

            [onedrive]
            client_id = "test-client-id"
        "#;

        let cfg: Config = toml::from_str(toml_str).expect("Failed to parse multi-folder config");
        assert_eq!(cfg.max_parallel_downloads, 8);
        assert_eq!(cfg.max_parallel_uploads, 3);

        let folders = cfg.get_sync_folders();
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[0].local_folder, PathBuf::from("/Users/test/Pictures"));
        assert_eq!(folders[0].remote_folder, "/Pictures");
        assert!(folders[0].enabled);

        assert_eq!(folders[1].local_folder, PathBuf::from("/Users/test/Notes"));
        assert_eq!(folders[1].remote_folder, "/Notes");
        assert!(!folders[1].enabled);
    }

    #[test]
    fn test_shared_parent_folder_with_subfolders() {
        let toml_str = r#"
            local_parent_folder = "/Users/franklinzhao/Library/CloudStorage/OneDrive-Personal"

            [[sync_folders]]
            subfolder = "SheridanFiles/Pictures"
            enabled = true

            [[sync_folders]]
            subfolder = "Documents/Projects"
            remote_folder = "/MyProjects"
            enabled = true

            [onedrive]
            client_id = "test-client-id"
        "#;

        let cfg: Config = toml::from_str(toml_str).expect("Failed to parse parent folder config");
        assert_eq!(
            cfg.local_parent_folder,
            Some(PathBuf::from("/Users/franklinzhao/Library/CloudStorage/OneDrive-Personal"))
        );

        let folders = cfg.get_sync_folders();
        assert_eq!(folders.len(), 2);
        assert_eq!(
            folders[0].local_folder,
            PathBuf::from("/Users/franklinzhao/Library/CloudStorage/OneDrive-Personal/SheridanFiles/Pictures")
        );
        assert_eq!(folders[0].remote_folder, "/SheridanFiles/Pictures");
        assert!(folders[0].enabled);

        assert_eq!(
            folders[1].local_folder,
            PathBuf::from("/Users/franklinzhao/Library/CloudStorage/OneDrive-Personal/Documents/Projects")
        );
        assert_eq!(folders[1].remote_folder, "/MyProjects");
        assert!(folders[1].enabled);
    }

    #[test]
    fn test_set_sync_folders_strips_parent_prefix() {
        let mut cfg = Config {
            local_parent_folder: Some(PathBuf::from("/Users/test/OneDrive")),
            sync_folders: Vec::new(),
            local_folder: None,
            remote_folder: None,
            sync_interval_secs: 300,
            exclude_patterns: Vec::new(),
            onedrive: OneDriveConfig {
                client_id: "test".to_string(),
                redirect_uri: "http://localhost:8765/callback".to_string(),
            },
            log_level: "info".to_string(),
            max_parallel_downloads: 4,
            max_parallel_uploads: 2,
        };

        cfg.set_sync_folders(vec![
            FolderPair {
                local_folder: PathBuf::from("/Users/test/OneDrive/Pictures"),
                remote_folder: "/Pictures".to_string(),
                enabled: true,
            }
        ]);

        assert_eq!(cfg.sync_folders.len(), 1);
        assert_eq!(cfg.sync_folders[0].subfolder.as_deref(), Some("Pictures"));
        assert_eq!(cfg.sync_folders[0].local_folder, None);
    }
}

