use anyhow::Result;
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::info;

/// The sync state of a single file
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncStatus {
    Synced,
    PendingUpload,
    PendingDownload,
    Conflict,
    Error,
}

impl std::fmt::Display for SyncStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncStatus::Synced          => write!(f, "synced"),
            SyncStatus::PendingUpload   => write!(f, "pending_upload"),
            SyncStatus::PendingDownload => write!(f, "pending_download"),
            SyncStatus::Conflict        => write!(f, "conflict"),
            SyncStatus::Error           => write!(f, "error"),
        }
    }
}

impl std::str::FromStr for SyncStatus {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "synced"           => Ok(SyncStatus::Synced),
            "pending_upload"   => Ok(SyncStatus::PendingUpload),
            "pending_download" => Ok(SyncStatus::PendingDownload),
            "conflict"         => Ok(SyncStatus::Conflict),
            "error"            => Ok(SyncStatus::Error),
            other              => Err(anyhow::anyhow!("Unknown status: {}", other)),
        }
    }
}

/// A tracked file entry in the state database
#[derive(Debug, Clone)]
pub struct FileRecord {
    /// Relative path from the sync root
    pub relative_path: String,
    /// SHA-256 hex digest of the local file content at last sync
    pub local_hash: Option<String>,
    /// OneDrive eTag at last sync
    pub remote_etag: Option<String>,
    /// OneDrive item ID
    pub remote_id: Option<String>,
    /// Last modification time seen on the remote
    pub remote_modified_at: Option<DateTime<Utc>>,
    /// Last modification time seen locally
    pub local_modified_at: Option<DateTime<Utc>>,
    pub status: SyncStatus,
}

/// Thin wrapper around a SQLite connection for sync state management.
///
/// The inner `Connection` is wrapped in `Mutex` so that `StateDb` is both
/// `Send` and `Sync`, allowing it to be shared across async tasks via `Arc`.
pub struct StateDb {
    conn: Mutex<Connection>,
}

// Safety: the Mutex guarantees exclusive access, making StateDb Send + Sync.
unsafe impl Send for StateDb {}
unsafe impl Sync for StateDb {}

impl StateDb {
    /// Open (or create) the state database at the given path
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let db = StateDb { conn: Mutex::new(conn) };
        db.migrate()?;
        Ok(db)
    }

    /// Default database path: ~/Library/Application Support/cloudsync/state.db
    #[allow(dead_code)]
    pub fn default_path() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("cloudsync")
            .join("state.db")
    }

    /// Return the database path specific to a given local sync folder root.
    pub fn path_for_folder(local_path: &Path) -> PathBuf {
        use sha2::{Digest, Sha256};
        let path_str = local_path.to_string_lossy();
        let mut hasher = Sha256::new();
        hasher.update(path_str.as_bytes());
        let hash = hex::encode(&hasher.finalize()[..4]);
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("cloudsync")
            .join(format!("state_{}.db", hash))
    }

    /// Run schema migrations
    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS files (
                relative_path       TEXT PRIMARY KEY,
                local_hash          TEXT,
                remote_etag         TEXT,
                remote_id           TEXT,
                remote_modified_at  TEXT,
                local_modified_at   TEXT,
                status              TEXT NOT NULL DEFAULT 'pending_upload',
                updated_at          TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE INDEX IF NOT EXISTS idx_files_remote_id ON files(remote_id);
        ")?;
        info!("State DB schema ready.");
        Ok(())
    }

    /// Upsert a file record
    pub fn upsert(&self, record: &FileRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO files
                (relative_path, local_hash, remote_etag, remote_id,
                 remote_modified_at, local_modified_at, status, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'))
             ON CONFLICT(relative_path) DO UPDATE SET
                local_hash         = excluded.local_hash,
                remote_etag        = excluded.remote_etag,
                remote_id          = excluded.remote_id,
                remote_modified_at = excluded.remote_modified_at,
                local_modified_at  = excluded.local_modified_at,
                status             = excluded.status,
                updated_at         = excluded.updated_at",
            params![
                record.relative_path,
                record.local_hash,
                record.remote_etag,
                record.remote_id,
                record.remote_modified_at.map(|t| t.to_rfc3339()),
                record.local_modified_at.map(|t| t.to_rfc3339()),
                record.status.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Look up a file record by relative path
    pub fn get(&self, relative_path: &str) -> Result<Option<FileRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT relative_path, local_hash, remote_etag, remote_id,
                    remote_modified_at, local_modified_at, status
             FROM files WHERE relative_path = ?1"
        )?;

        let result = stmt.query_row(params![relative_path], |row| {
            Ok(FileRecord {
                relative_path:      row.get(0)?,
                local_hash:         row.get(1)?,
                remote_etag:        row.get(2)?,
                remote_id:          row.get(3)?,
                remote_modified_at: row.get::<_, Option<String>>(4)?
                    .and_then(|s| s.parse().ok()),
                local_modified_at:  row.get::<_, Option<String>>(5)?
                    .and_then(|s| s.parse().ok()),
                status: row.get::<_, String>(6)?
                    .parse()
                    .unwrap_or(SyncStatus::Error),
            })
        });

        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Find a file record by its remote item ID
    pub fn find_by_remote_id(&self, remote_id: &str) -> Result<Option<FileRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT relative_path, local_hash, remote_etag, remote_id,
                    remote_modified_at, local_modified_at, status
             FROM files WHERE remote_id = ?1 LIMIT 1"
        )?;

        let result = stmt.query_row(params![remote_id], |row| {
            Ok(FileRecord {
                relative_path:      row.get(0)?,
                local_hash:         row.get(1)?,
                remote_etag:        row.get(2)?,
                remote_id:          row.get(3)?,
                remote_modified_at: row.get::<_, Option<String>>(4)?
                    .and_then(|s| s.parse().ok()),
                local_modified_at:  row.get::<_, Option<String>>(5)?
                    .and_then(|s| s.parse().ok()),
                status: row.get::<_, String>(6)?
                    .parse()
                    .unwrap_or(SyncStatus::Error),
            })
        });

        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Return all tracked file records
    pub fn all_records(&self) -> Result<Vec<FileRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT relative_path, local_hash, remote_etag, remote_id,
                    remote_modified_at, local_modified_at, status
             FROM files"
        )?;

        let records = stmt.query_map([], |row| {
            Ok(FileRecord {
                relative_path:      row.get(0)?,
                local_hash:         row.get(1)?,
                remote_etag:        row.get(2)?,
                remote_id:          row.get(3)?,
                remote_modified_at: row.get::<_, Option<String>>(4)?
                    .and_then(|s| s.parse().ok()),
                local_modified_at:  row.get::<_, Option<String>>(5)?
                    .and_then(|s| s.parse().ok()),
                status: row.get::<_, String>(6)?
                    .parse()
                    .unwrap_or(SyncStatus::Error),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

        Ok(records)
    }

    /// Get all records with a given status
    #[allow(dead_code)]
    pub fn get_by_status(&self, status: &SyncStatus) -> Result<Vec<FileRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT relative_path, local_hash, remote_etag, remote_id,
                    remote_modified_at, local_modified_at, status
             FROM files WHERE status = ?1"
        )?;

        let records = stmt.query_map(params![status.to_string()], |row| {
            Ok(FileRecord {
                relative_path:      row.get(0)?,
                local_hash:         row.get(1)?,
                remote_etag:        row.get(2)?,
                remote_id:          row.get(3)?,
                remote_modified_at: row.get::<_, Option<String>>(4)?
                    .and_then(|s| s.parse().ok()),
                local_modified_at:  row.get::<_, Option<String>>(5)?
                    .and_then(|s| s.parse().ok()),
                status: row.get::<_, String>(6)?
                    .parse()
                    .unwrap_or(SyncStatus::Error),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

        Ok(records)
    }

    /// Mark a file as synced with updated hashes / etags
    #[allow(dead_code)]
    pub fn mark_synced(
        &self,
        relative_path: &str,
        local_hash: &str,
        remote_etag: &str,
        remote_id: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE files SET
                local_hash = ?1,
                remote_etag = ?2,
                remote_id = ?3,
                status = 'synced',
                updated_at = datetime('now')
             WHERE relative_path = ?4",
            params![local_hash, remote_etag, remote_id, relative_path],
        )?;
        Ok(())
    }

    /// Remove a file record (e.g., after a delete is synced)
    pub fn remove(&self, relative_path: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM files WHERE relative_path = ?1",
            params![relative_path],
        )?;
        Ok(())
    }

    /// Return all tracked relative paths
    #[allow(dead_code)]
    pub fn all_paths(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT relative_path FROM files")?;
        let paths = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(paths)
    }
}
