use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::PathBuf;

use crate::config::AppConfig;
use crate::models::{Freshness, Repository, ScanSource};

/// Database management
pub struct Database {
    conn: Connection,
}

impl Database {
    /// Get database path
    pub fn db_path() -> Result<PathBuf> {
        let dir = AppConfig::config_dir()?;
        Ok(dir.join("getlatestrepo.db"))
    }

    /// Open database connection
    pub fn open() -> Result<Self> {
        let path = Self::db_path()?;
        let conn = Connection::open(&path)
            .with_context(|| format!("无法打开数据库: {}", path.display()))?;

        // Enable WAL mode for better concurrency performance
        let _journal_mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        // Set busy timeout to avoid "database is locked" errors
        let _old_timeout: i32 = conn.query_row("PRAGMA busy_timeout = 5000", [], |row| row.get(0))?;
        // WAL mode optimization: synchronous = NORMAL for better performance
        conn.execute("PRAGMA synchronous = NORMAL", [])?;

        #[cfg(unix)]
        {
            use std::fs;
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
                eprintln!("警告：设置数据库文件权限失败: {}", e);
            }
        }

        let db = Self { conn };
        db.init_tables()?;
        
        Ok(db)
    }

    const SCHEMA_VERSION: i32 = 1;

    /// Initialize table schema and run migrations
    fn init_tables(&self) -> Result<()> {
        let version: i32 = self.conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        
        // Scan sources table
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS scan_sources (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                root_path TEXT NOT NULL UNIQUE,
                max_depth INTEGER NOT NULL DEFAULT 5,
                ignore_patterns TEXT NOT NULL DEFAULT '',
                follow_symlinks INTEGER NOT NULL DEFAULT 0,
                enabled INTEGER NOT NULL DEFAULT 1,
                last_scan_at TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )",
            [],
        )?;

        // Repositories table
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS repositories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                root_path TEXT NOT NULL,
                name TEXT NOT NULL,
                depth INTEGER NOT NULL DEFAULT 0,
                
                branch TEXT,
                dirty INTEGER NOT NULL DEFAULT 0,
                dirty_files TEXT,
                upstream_ref TEXT,
                upstream_url TEXT,
                
                ahead_count INTEGER NOT NULL DEFAULT 0,
                behind_count INTEGER NOT NULL DEFAULT 0,
                freshness TEXT NOT NULL DEFAULT 'no_remote',
                
                last_commit_at TIMESTAMP,
                last_commit_message TEXT,
                last_commit_author TEXT,
                
                last_scanned_at TIMESTAMP,
                last_fetch_at TIMESTAMP,
                last_pull_at TIMESTAMP,
                
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )",
            [],
        )?;

        // Indexes
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_repo_root ON repositories(root_path)",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_repo_freshness ON repositories(freshness)",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_repo_updated ON repositories(updated_at)",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_repo_root_freshness ON repositories(root_path, freshness)",
            [],
        )?;

        // Run migrations if needed
        if version < Self::SCHEMA_VERSION {
            // Future schema migrations go here
            // e.g. ALTER TABLE ADD COLUMN ...

            self.conn.execute_batch(
                &format!("PRAGMA user_version = {};", Self::SCHEMA_VERSION),
            )?;
        }

        Ok(())
    }

    // ==================== Scan Sources ====================

    /// Insert or update scan source
    pub fn upsert_scan_source(&self, source: &mut ScanSource) -> Result<()> {
        let ignore_patterns = source.ignore_patterns.join(",");
        
        if let Some(id) = source.id {
            self.conn.execute(
                "UPDATE scan_sources SET
                    root_path = ?1,
                    max_depth = ?2,
                    ignore_patterns = ?3,
                    follow_symlinks = ?4,
                    enabled = ?5,
                    last_scan_at = ?6,
                    updated_at = CURRENT_TIMESTAMP
                WHERE id = ?7",
                params![
                    source.root_path,
                    source.max_depth,
                    ignore_patterns,
                    source.follow_symlinks as i32,
                    source.enabled as i32,
                    source.last_scan_at,
                    id
                ],
            )?;
        } else {
            self.conn.execute(
                "INSERT INTO scan_sources
                    (root_path, max_depth, ignore_patterns, follow_symlinks, enabled, last_scan_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(root_path) DO UPDATE SET
                    max_depth = excluded.max_depth,
                    ignore_patterns = excluded.ignore_patterns,
                    follow_symlinks = excluded.follow_symlinks,
                    enabled = excluded.enabled,
                    last_scan_at = excluded.last_scan_at,
                    updated_at = CURRENT_TIMESTAMP",
                params![
                    source.root_path,
                    source.max_depth,
                    ignore_patterns,
                    source.follow_symlinks as i32,
                    source.enabled as i32,
                    source.last_scan_at
                ],
            )?;
            
            source.id = Some(self.conn.last_insert_rowid());
        }
        
        Ok(())
    }

    /// Get all scan sources
    /// 
    /// Currently unused, reserved for future scan source management
    #[allow(dead_code)]
    pub fn list_scan_sources(&self) -> Result<Vec<ScanSource>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, root_path, max_depth, ignore_patterns, follow_symlinks, 
                    enabled, last_scan_at
             FROM scan_sources
             WHERE enabled = 1
             ORDER BY root_path"
        )?;

        let sources = stmt.query_map([], |row| {
            let ignore_str: String = row.get(3)?;
            let ignore_patterns = ignore_str.split(',').map(|s| s.trim().to_string()).collect();
            
            Ok(ScanSource {
                id: row.get(0)?,
                root_path: row.get(1)?,
                max_depth: row.get::<_, usize>(2)?,
                ignore_patterns,
                follow_symlinks: row.get::<_, i32>(4)? != 0,
                enabled: row.get::<_, i32>(5)? != 0,
                last_scan_at: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

        Ok(sources)
    }

    /// Delete scan source
    #[allow(dead_code)]
    pub fn delete_scan_source(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM scan_sources WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Delete scan source by root path.
    ///
    /// The CLI displays scan sources from the TOML config, where SQLite row IDs are
    /// not persisted back. Deleting by canonical root path keeps config and DB sync
    /// deterministic even when the user removes an item by displayed list number.
    pub fn delete_scan_source_by_path(&self, root_path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM scan_sources WHERE root_path = ?1",
            params![root_path],
        )?;
        Ok(())
    }

    // ==================== Repositories ====================

    const UPSERT_REPO_SQL: &str = "INSERT INTO repositories
        (path, root_path, name, depth, branch, dirty, dirty_files,
         upstream_ref, upstream_url, ahead_count, behind_count, freshness,
         last_commit_at, last_commit_message, last_commit_author,
         last_scanned_at, last_fetch_at, last_pull_at)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
    ON CONFLICT(path) DO UPDATE SET
        root_path = excluded.root_path,
        name = excluded.name,
        depth = excluded.depth,
        branch = excluded.branch,
        dirty = excluded.dirty,
        dirty_files = excluded.dirty_files,
        upstream_ref = excluded.upstream_ref,
        upstream_url = excluded.upstream_url,
        ahead_count = excluded.ahead_count,
        behind_count = excluded.behind_count,
        freshness = excluded.freshness,
        last_commit_at = excluded.last_commit_at,
        last_commit_message = excluded.last_commit_message,
        last_commit_author = excluded.last_commit_author,
        last_scanned_at = excluded.last_scanned_at,
        last_fetch_at = excluded.last_fetch_at,
        last_pull_at = excluded.last_pull_at,
        updated_at = CURRENT_TIMESTAMP";

    fn build_repo_params(repo: &Repository) -> (String, &str) {
        let dirty_files = serde_json::to_string(&repo.dirty_files).unwrap_or_else(|_| "[]".to_string());
        let freshness_str = repo.freshness.as_str();
        (dirty_files, freshness_str)
    }

    /// Insert or update repository
    pub fn upsert_repository(&self, repo: &mut Repository) -> Result<()> {
        let (dirty_files, freshness_str) = Self::build_repo_params(repo);

        self.conn.execute(
            Self::UPSERT_REPO_SQL,
            params![
                repo.path, repo.root_path, repo.name, repo.depth, repo.branch,
                repo.dirty as i32, dirty_files, repo.upstream_ref, repo.upstream_url,
                repo.ahead_count, repo.behind_count, freshness_str,
                repo.last_commit_at, repo.last_commit_message, repo.last_commit_author,
                repo.last_scanned_at, repo.last_fetch_at, repo.last_pull_at,
            ],
        )?;

        if repo.id.is_none() {
            repo.id = Some(self.conn.last_insert_rowid());
        }

        Ok(())
    }

    /// Get single repository
    pub fn get_repository(&self, path: &str) -> Result<Option<Repository>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, root_path, name, depth, branch, dirty, dirty_files,
                    upstream_ref, upstream_url, ahead_count, behind_count, freshness,
                    last_commit_at, last_commit_message, last_commit_author,
                    last_scanned_at, last_fetch_at, last_pull_at
             FROM repositories WHERE path = ?1"
        )?;

        let repo = stmt.query_row(params![path], Self::row_to_repository).optional()?;
        Ok(repo)
    }

    /// Get all repositories
    pub fn list_repositories(&self) -> Result<Vec<Repository>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, root_path, name, depth, branch, dirty, dirty_files,
                    upstream_ref, upstream_url, ahead_count, behind_count, freshness,
                    last_commit_at, last_commit_message, last_commit_author,
                    last_scanned_at, last_fetch_at, last_pull_at
             FROM repositories
             ORDER BY last_commit_at DESC NULLS LAST, updated_at DESC"
        )?;

        let repos = stmt.query_map([], Self::row_to_repository)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(repos)
    }

    /// Update fetch time
    pub fn update_fetch_time(&self, path: &str) -> Result<()> {
        let now = chrono::Local::now();
        self.conn.execute(
            "UPDATE repositories SET last_fetch_at = ?1 WHERE path = ?2",
            params![now, path],
        )?;
        Ok(())
    }

    /// Update pull time
    pub fn update_pull_time(&self, path: &str) -> Result<()> {
        let now = chrono::Local::now();
        self.conn.execute(
            "UPDATE repositories SET last_pull_at = ?1 WHERE path = ?2",
            params![now, path],
        )?;
        Ok(())
    }

    /// Delete repositories under the specified root path (cleanup deleted repos)
    /// 
    /// Currently unused, reserved for future bulk cleanup functionality
    #[allow(dead_code)]
    pub fn delete_repositories_by_root(&self, root_path: &str) -> Result<usize> {
        let count = self.conn.execute(
            "DELETE FROM repositories WHERE root_path = ?1",
            params![root_path],
        )?;
        Ok(count)
    }

    /// Delete repository at specified path
    pub fn delete_repository(&self, path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM repositories WHERE path = ?1",
            params![path],
        )?;
        Ok(())
    }

    /// Atomically move repository record (delete old path, insert new path)
    pub fn move_repository(&self, old_path: &str, repo: &mut Repository) -> Result<()> {
        // 防御性检查：确保当前不在事务中，避免嵌套事务导致不可预期行为
        if !self.conn.is_autocommit() {
            anyhow::bail!("当前已在事务中，无法移动仓库记录");
        }
        // Use immediate_transaction to ensure correct behavior under WAL mode
        // Use new_unchecked because rusqlite requires &mut self for transaction().
        // This is safe here because there are no nested transactions in the current call graph.
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        
        // Delete old record
        tx.execute(
            "DELETE FROM repositories WHERE path = ?1",
            params![old_path],
        )?;
        
        // Insert new record
        let (dirty_files, freshness_str) = Self::build_repo_params(repo);
        
        tx.execute(
            Self::UPSERT_REPO_SQL,
            params![
                repo.path, repo.root_path, repo.name, repo.depth, repo.branch,
                repo.dirty as i32, dirty_files, repo.upstream_ref, repo.upstream_url,
                repo.ahead_count, repo.behind_count, freshness_str,
                repo.last_commit_at, repo.last_commit_message, repo.last_commit_author,
                repo.last_scanned_at, repo.last_fetch_at, repo.last_pull_at,
            ],
        )?;
        
        tx.commit()?;
        Ok(())
    }

    /// Row to repository object
    fn row_to_repository(row: &rusqlite::Row) -> Result<Repository, rusqlite::Error> {
        let dirty_files_str: String = row.get("dirty_files")?;
        let dirty_files = if dirty_files_str.starts_with('[') {
            serde_json::from_str(&dirty_files_str).unwrap_or_default()
        } else {
            // Backward compatibility: old newline-separated format
            dirty_files_str.lines().map(|s| s.to_string()).collect()
        };
        let freshness_str: String = row.get("freshness")?;

        Ok(Repository {
            id: row.get("id")?,
            path: row.get("path")?,
            root_path: row.get("root_path")?,
            name: row.get("name")?,
            depth: row.get("depth")?,
            branch: row.get("branch")?,
            dirty: row.get::<_, i32>("dirty")? != 0,
            file_changes: Vec::new(), // Re-scan to get when restoring from database
            dirty_files,
            upstream_ref: row.get("upstream_ref")?,
            upstream_url: row.get("upstream_url")?,
            ahead_count: row.get("ahead_count")?,
            behind_count: row.get("behind_count")?,
            freshness: Freshness::from(freshness_str.as_str()),
            last_commit_at: row.get("last_commit_at")?,
            last_commit_message: row.get("last_commit_message")?,
            last_commit_author: row.get("last_commit_author")?,
            last_scanned_at: row.get("last_scanned_at")?,
            last_fetch_at: row.get("last_fetch_at")?,
            last_pull_at: row.get("last_pull_at")?,
        })
    }
}
