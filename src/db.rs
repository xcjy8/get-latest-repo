use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;

use crate::config::AppConfig;
use crate::models::{Freshness, Repository, ScanSource};

/// Database management
pub struct Database {
    conn: Connection,
}

/// Web 长任务的持久化批次记录；SQLite 是跨重启的唯一权威状态源。
#[derive(Debug, Clone, Default)]
pub struct OperationBatchRecord {
    pub batch_id: String,
    pub request_id: String,
    pub kind: String,
    pub state: String,
    pub message: String,
    pub details_json: String,
    pub total: usize,
    pub completed: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub partial: usize,
    pub no_action: usize,
    pub skipped: usize,
    pub source_batch_id: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// 单仓库、单阶段执行结果；同一批次内按仓库路径和阶段幂等更新。
#[derive(Debug, Clone, Default)]
pub struct OperationItemRecord {
    pub batch_id: String,
    pub repo_id: Option<i64>,
    pub repo_path: String,
    pub repo_name: String,
    pub stage: String,
    pub outcome: String,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub parent_synced: bool,
    pub final_dirty: bool,
}

impl Database {
    #[cfg(test)]
    pub(crate) fn open_in_memory_for_test() -> Self {
        let db = Self {
            conn: Connection::open_in_memory().expect("测试数据库必须可创建"),
        };
        db.init_tables().expect("测试数据库表必须可初始化");
        db
    }

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
        let _journal_mode: String =
            conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        // Set busy timeout to avoid "database is locked" errors
        let _old_timeout: i32 =
            conn.query_row("PRAGMA busy_timeout = 5000", [], |row| row.get(0))?;
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

    const SCHEMA_VERSION: i32 = 2;

    /// Initialize table schema and run migrations
    fn init_tables(&self) -> Result<()> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;

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

        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS operation_batches (
                batch_id TEXT PRIMARY KEY,
                request_id TEXT NOT NULL UNIQUE,
                kind TEXT NOT NULL,
                state TEXT NOT NULL,
                message TEXT NOT NULL,
                details_json TEXT NOT NULL DEFAULT '[]',
                total INTEGER NOT NULL DEFAULT 0,
                completed INTEGER NOT NULL DEFAULT 0,
                succeeded INTEGER NOT NULL DEFAULT 0,
                failed INTEGER NOT NULL DEFAULT 0,
                partial INTEGER NOT NULL DEFAULT 0,
                no_action INTEGER NOT NULL DEFAULT 0,
                skipped INTEGER NOT NULL DEFAULT 0,
                source_batch_id TEXT,
                started_at TEXT,
                finished_at TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX IF NOT EXISTS idx_operation_batches_kind_created
                ON operation_batches(kind, created_at DESC);
            CREATE TABLE IF NOT EXISTS operation_items (
                batch_id TEXT NOT NULL,
                repo_id INTEGER,
                repo_path TEXT NOT NULL,
                repo_name TEXT NOT NULL,
                stage TEXT NOT NULL,
                outcome TEXT NOT NULL,
                error_code TEXT,
                error_detail TEXT,
                parent_synced INTEGER NOT NULL DEFAULT 0,
                final_dirty INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY(batch_id, repo_path, stage),
                FOREIGN KEY(batch_id) REFERENCES operation_batches(batch_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_operation_items_batch_outcome
                ON operation_items(batch_id, stage, outcome);",
        )?;

        // Run migrations if needed
        if version < Self::SCHEMA_VERSION {
            // Future schema migrations go here
            // e.g. ALTER TABLE ADD COLUMN ...

            self.conn
                .execute_batch(&format!("PRAGMA user_version = {};", Self::SCHEMA_VERSION))?;
        }

        Ok(())
    }

    // ==================== Scan Sources ====================

    /// 使用指定连接插入或更新扫描源，供普通写入与事务批量写入复用。
    fn upsert_scan_source_with_connection(
        connection: &Connection,
        source: &mut ScanSource,
    ) -> Result<()> {
        let ignore_patterns = source.ignore_patterns.join(",");
        let max_depth = i64::try_from(source.max_depth).context("扫描深度超出数据库整数范围")?;

        if let Some(id) = source.id {
            connection.execute(
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
                    max_depth,
                    ignore_patterns,
                    source.follow_symlinks as i32,
                    source.enabled as i32,
                    source.last_scan_at,
                    id
                ],
            )?;
        } else {
            source.id = Some(connection.query_row(
                "INSERT INTO scan_sources
                    (root_path, max_depth, ignore_patterns, follow_symlinks, enabled, last_scan_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(root_path) DO UPDATE SET
                    max_depth = excluded.max_depth,
                    ignore_patterns = excluded.ignore_patterns,
                    follow_symlinks = excluded.follow_symlinks,
                    enabled = excluded.enabled,
                    last_scan_at = excluded.last_scan_at,
                    updated_at = CURRENT_TIMESTAMP
                RETURNING id",
                params![
                    source.root_path,
                    max_depth,
                    ignore_patterns,
                    source.follow_symlinks as i32,
                    source.enabled as i32,
                    source.last_scan_at
                ],
                |row| row.get(0),
            )?);
        }

        Ok(())
    }

    /// 插入或更新单个扫描源。
    pub fn upsert_scan_source(&self, source: &mut ScanSource) -> Result<()> {
        Self::upsert_scan_source_with_connection(&self.conn, source)
    }

    /// 在同一数据库事务中同步扫描源，并在提交前执行配置持久化。
    ///
    /// 配置持久化失败会使数据库事务自动回滚；数据库提交失败则由调用方恢复旧配置。
    pub fn commit_scan_sources_with<F>(
        &mut self,
        sources: &mut [ScanSource],
        persist_config: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        let transaction = self.conn.transaction()?;
        for source in sources {
            Self::upsert_scan_source_with_connection(&transaction, source)?;
        }
        persist_config()?;
        transaction.commit()?;
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
             ORDER BY root_path",
        )?;

        let sources = stmt
            .query_map([], |row| {
                let ignore_str: String = row.get(3)?;
                let ignore_patterns = ignore_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .collect();

                let max_depth_value: i64 = row.get(2)?;
                let max_depth = usize::try_from(max_depth_value).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Integer,
                        Box::new(error),
                    )
                })?;
                Ok(ScanSource {
                    id: row.get(0)?,
                    root_path: row.get(1)?,
                    max_depth,
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
        self.conn
            .execute("DELETE FROM scan_sources WHERE id = ?1", params![id])?;
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
        last_fetch_at = COALESCE(excluded.last_fetch_at, repositories.last_fetch_at),
        last_pull_at = COALESCE(excluded.last_pull_at, repositories.last_pull_at),
        updated_at = CURRENT_TIMESTAMP";

    fn build_repo_params(repo: &Repository) -> (String, &str) {
        let dirty_files =
            serde_json::to_string(&repo.dirty_files).unwrap_or_else(|_| "[]".to_string());
        let freshness_str = repo.freshness.as_str();
        (dirty_files, freshness_str)
    }

    /// Insert or update repository
    pub fn upsert_repository(&self, repo: &mut Repository) -> Result<()> {
        let (dirty_files, freshness_str) = Self::build_repo_params(repo);

        let sql = format!("{} RETURNING id", Self::UPSERT_REPO_SQL);
        repo.id = Some(self.conn.query_row(
            &sql,
            params![
                repo.path,
                repo.root_path,
                repo.name,
                repo.depth,
                repo.branch,
                repo.dirty as i32,
                dirty_files,
                repo.upstream_ref,
                repo.upstream_url,
                repo.ahead_count,
                repo.behind_count,
                freshness_str,
                repo.last_commit_at,
                repo.last_commit_message,
                repo.last_commit_author,
                repo.last_scanned_at,
                repo.last_fetch_at,
                repo.last_pull_at,
            ],
            |row| row.get(0),
        )?);

        Ok(())
    }

    /// Get single repository
    pub fn get_repository(&self, path: &str) -> Result<Option<Repository>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, root_path, name, depth, branch, dirty, dirty_files,
                    upstream_ref, upstream_url, ahead_count, behind_count, freshness,
                    last_commit_at, last_commit_message, last_commit_author,
                    last_scanned_at, last_fetch_at, last_pull_at
             FROM repositories WHERE path = ?1",
        )?;

        let repo = stmt
            .query_row(params![path], Self::row_to_repository)
            .optional()?;
        Ok(repo)
    }

    /// 按稳定数据库 ID 读取仓库，供 Web API 使用，避免接受任意文件系统路径。
    pub fn get_repository_by_id(&self, id: i64) -> Result<Option<Repository>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, root_path, name, depth, branch, dirty, dirty_files,
                    upstream_ref, upstream_url, ahead_count, behind_count, freshness,
                    last_commit_at, last_commit_message, last_commit_author,
                    last_scanned_at, last_fetch_at, last_pull_at
             FROM repositories WHERE id = ?1",
        )?;
        Ok(stmt
            .query_row(params![id], Self::row_to_repository)
            .optional()?)
    }

    /// Get all repositories
    pub fn list_repositories(&self) -> Result<Vec<Repository>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, root_path, name, depth, branch, dirty, dirty_files,
                    upstream_ref, upstream_url, ahead_count, behind_count, freshness,
                    last_commit_at, last_commit_message, last_commit_author,
                    last_scanned_at, last_fetch_at, last_pull_at
             FROM repositories
             ORDER BY last_commit_at DESC NULLS LAST, updated_at DESC",
        )?;

        let repos = stmt
            .query_map([], Self::row_to_repository)?
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

    // ==================== 持久化操作批次 ====================

    pub fn upsert_operation_batch(&self, batch: &OperationBatchRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO operation_batches
                (batch_id, request_id, kind, state, message, details_json, total, completed,
                 succeeded, failed, partial, no_action, skipped, source_batch_id,
                 started_at, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT(batch_id) DO UPDATE SET
                state = excluded.state,
                message = excluded.message,
                details_json = excluded.details_json,
                total = excluded.total,
                completed = excluded.completed,
                succeeded = excluded.succeeded,
                failed = excluded.failed,
                partial = excluded.partial,
                no_action = excluded.no_action,
                skipped = excluded.skipped,
                source_batch_id = excluded.source_batch_id,
                started_at = excluded.started_at,
                finished_at = excluded.finished_at,
                updated_at = CURRENT_TIMESTAMP",
            params![
                batch.batch_id,
                batch.request_id,
                batch.kind,
                batch.state,
                batch.message,
                batch.details_json,
                i64::try_from(batch.total)?,
                i64::try_from(batch.completed)?,
                i64::try_from(batch.succeeded)?,
                i64::try_from(batch.failed)?,
                i64::try_from(batch.partial)?,
                i64::try_from(batch.no_action)?,
                i64::try_from(batch.skipped)?,
                batch.source_batch_id,
                batch.started_at,
                batch.finished_at,
            ],
        )?;
        Ok(())
    }

    fn row_to_operation_batch(
        row: &rusqlite::Row,
    ) -> Result<OperationBatchRecord, rusqlite::Error> {
        fn usize_column(row: &rusqlite::Row, name: &str) -> Result<usize, rusqlite::Error> {
            let value: i64 = row.get(name)?;
            usize::try_from(value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })
        }

        Ok(OperationBatchRecord {
            batch_id: row.get("batch_id")?,
            request_id: row.get("request_id")?,
            kind: row.get("kind")?,
            state: row.get("state")?,
            message: row.get("message")?,
            details_json: row.get("details_json")?,
            total: usize_column(row, "total")?,
            completed: usize_column(row, "completed")?,
            succeeded: usize_column(row, "succeeded")?,
            failed: usize_column(row, "failed")?,
            partial: usize_column(row, "partial")?,
            no_action: usize_column(row, "no_action")?,
            skipped: usize_column(row, "skipped")?,
            source_batch_id: row.get("source_batch_id")?,
            started_at: row.get("started_at")?,
            finished_at: row.get("finished_at")?,
        })
    }

    pub fn latest_operation_batch(&self) -> Result<Option<OperationBatchRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT batch_id, request_id, kind, state, message, details_json, total,
                        completed, succeeded, failed, partial, no_action, skipped,
                        source_batch_id, started_at, finished_at
                 FROM operation_batches ORDER BY created_at DESC, rowid DESC LIMIT 1",
                [],
                Self::row_to_operation_batch,
            )
            .optional()?)
    }

    pub fn operation_batch_by_request_id(
        &self,
        request_id: &str,
    ) -> Result<Option<OperationBatchRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT batch_id, request_id, kind, state, message, details_json, total,
                        completed, succeeded, failed, partial, no_action, skipped,
                        source_batch_id, started_at, finished_at
                 FROM operation_batches WHERE request_id = ?1",
                params![request_id],
                Self::row_to_operation_batch,
            )
            .optional()?)
    }

    pub fn latest_fetch_batch(&self) -> Result<Option<OperationBatchRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT batch_id, request_id, kind, state, message, details_json, total,
                        completed, succeeded, failed, partial, no_action, skipped,
                        source_batch_id, started_at, finished_at
                 FROM operation_batches
                 WHERE kind = 'fetch' AND state IN ('succeeded', 'partial_failed')
                 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                [],
                Self::row_to_operation_batch,
            )
            .optional()?)
    }

    /// 判断 Fetch 批次是否已被任一更新操作消费；同一远程快照只允许更新一次。
    pub fn fetch_batch_has_consumer(&self, batch_id: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM operation_batches
                WHERE source_batch_id = ?1
                  AND kind IN ('pull-safe', 'pull-force', 'pull-backup')
            )",
            params![batch_id],
            |row| row.get::<_, bool>(0),
        )?)
    }

    pub fn upsert_operation_item(&self, item: &OperationItemRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO operation_items
                (batch_id, repo_id, repo_path, repo_name, stage, outcome, error_code,
                 error_detail, parent_synced, final_dirty)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(batch_id, repo_path, stage) DO UPDATE SET
                repo_id = excluded.repo_id,
                repo_name = excluded.repo_name,
                outcome = excluded.outcome,
                error_code = excluded.error_code,
                error_detail = excluded.error_detail,
                parent_synced = excluded.parent_synced,
                final_dirty = excluded.final_dirty,
                updated_at = CURRENT_TIMESTAMP",
            params![
                item.batch_id,
                item.repo_id,
                item.repo_path,
                item.repo_name,
                item.stage,
                item.outcome,
                item.error_code,
                item.error_detail,
                item.parent_synced as i32,
                item.final_dirty as i32,
            ],
        )?;
        Ok(())
    }

    pub fn successful_fetch_repositories(&self, batch_id: &str) -> Result<Vec<Repository>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.id, r.path, r.root_path, r.name, r.depth, r.branch, r.dirty,
                    r.dirty_files, r.upstream_ref, r.upstream_url, r.ahead_count,
                    r.behind_count, r.freshness, r.last_commit_at, r.last_commit_message,
                    r.last_commit_author, r.last_scanned_at, r.last_fetch_at, r.last_pull_at
             FROM operation_items i
             JOIN repositories r ON r.path = i.repo_path
             WHERE i.batch_id = ?1 AND i.stage = 'fetch' AND i.outcome = 'succeeded'
             ORDER BY r.path",
        )?;
        Ok(stmt
            .query_map(params![batch_id], Self::row_to_repository)?
            .collect::<Result<Vec<_>, _>>()?)
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
        self.conn
            .execute("DELETE FROM repositories WHERE path = ?1", params![path])?;
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
                repo.path,
                repo.root_path,
                repo.name,
                repo.depth,
                repo.branch,
                repo.dirty as i32,
                dirty_files,
                repo.upstream_ref,
                repo.upstream_url,
                repo.ahead_count,
                repo.behind_count,
                freshness_str,
                repo.last_commit_at,
                repo.last_commit_message,
                repo.last_commit_author,
                repo.last_scanned_at,
                repo.last_fetch_at,
                repo.last_pull_at,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_database() -> Database {
        Database::open_in_memory_for_test()
    }

    #[test]
    fn scan_upsert_preserves_fetch_and_pull_timestamps() {
        let db = in_memory_database();
        let fetch_time = chrono::Local::now();
        let pull_time = fetch_time + chrono::Duration::seconds(1);
        let mut existing = Repository {
            path: "/tmp/repo".to_string(),
            root_path: "/tmp".to_string(),
            name: "repo".to_string(),
            last_fetch_at: Some(fetch_time),
            last_pull_at: Some(pull_time),
            ..Default::default()
        };
        db.upsert_repository(&mut existing).unwrap();

        let mut scanned = Repository {
            path: existing.path.clone(),
            root_path: existing.root_path.clone(),
            name: existing.name.clone(),
            ..Default::default()
        };
        db.upsert_repository(&mut scanned).unwrap();

        let saved = db.get_repository(&existing.path).unwrap().unwrap();
        assert_eq!(saved.last_fetch_at, Some(fetch_time));
        assert_eq!(saved.last_pull_at, Some(pull_time));
    }

    #[test]
    fn scan_source_upsert_returns_conflicting_row_id() {
        let db = in_memory_database();
        let mut first = ScanSource {
            root_path: "/repos/first".to_string(),
            ..ScanSource::default()
        };
        let mut second = ScanSource {
            root_path: "/repos/second".to_string(),
            ..ScanSource::default()
        };
        db.upsert_scan_source(&mut first).unwrap();
        db.upsert_scan_source(&mut second).unwrap();
        let expected_id = first.id;
        let mut conflicting = ScanSource {
            root_path: first.root_path.clone(),
            ..ScanSource::default()
        };

        db.upsert_scan_source(&mut conflicting).unwrap();

        assert_eq!(conflicting.id, expected_id);
        assert_ne!(conflicting.id, second.id);
    }

    #[test]
    fn scan_source_transaction_rolls_back_when_config_persistence_fails() {
        let mut db = in_memory_database();
        let mut sources = vec![ScanSource {
            root_path: "/repos/transaction".to_string(),
            ..ScanSource::default()
        }];

        let result =
            db.commit_scan_sources_with(&mut sources, || anyhow::bail!("模拟配置写入失败"));

        assert!(result.is_err());
        assert!(db.list_scan_sources().unwrap().is_empty());
    }

    #[test]
    fn repository_upsert_returns_conflicting_row_id() {
        let db = in_memory_database();
        let mut first = Repository {
            path: "/repos/first".to_string(),
            root_path: "/repos".to_string(),
            name: "first".to_string(),
            ..Repository::default()
        };
        let mut second = Repository {
            path: "/repos/second".to_string(),
            root_path: "/repos".to_string(),
            name: "second".to_string(),
            ..Repository::default()
        };
        db.upsert_repository(&mut first).unwrap();
        db.upsert_repository(&mut second).unwrap();
        let expected_id = first.id;
        let mut conflicting = Repository {
            path: first.path.clone(),
            root_path: first.root_path.clone(),
            name: first.name.clone(),
            ..Repository::default()
        };

        db.upsert_repository(&mut conflicting).unwrap();

        assert_eq!(conflicting.id, expected_id);
        assert_ne!(conflicting.id, second.id);
    }

    #[test]
    fn operation_batch_and_fetch_scope_survive_database_reads() {
        let db = in_memory_database();
        let mut repository = Repository {
            path: "/repos/durable".to_string(),
            root_path: "/repos".to_string(),
            name: "durable".to_string(),
            ..Repository::default()
        };
        db.upsert_repository(&mut repository).unwrap();
        db.upsert_operation_batch(&OperationBatchRecord {
            batch_id: "batch-fetch".to_string(),
            request_id: "request-fetch".to_string(),
            kind: "fetch".to_string(),
            state: "partial_failed".to_string(),
            message: "获取完成".to_string(),
            details_json: "[]".to_string(),
            total: 2,
            completed: 2,
            succeeded: 1,
            failed: 1,
            ..OperationBatchRecord::default()
        })
        .unwrap();
        db.upsert_operation_item(&OperationItemRecord {
            batch_id: "batch-fetch".to_string(),
            repo_id: repository.id,
            repo_path: repository.path.clone(),
            repo_name: repository.name.clone(),
            stage: "fetch".to_string(),
            outcome: "succeeded".to_string(),
            ..OperationItemRecord::default()
        })
        .unwrap();

        let latest = db.latest_fetch_batch().unwrap().unwrap();
        let scope = db.successful_fetch_repositories(&latest.batch_id).unwrap();

        assert_eq!(latest.succeeded, 1);
        assert_eq!(latest.failed, 1);
        assert_eq!(scope.len(), 1);
        assert_eq!(scope[0].path, repository.path);
    }

    #[test]
    fn operation_request_id_is_idempotent() {
        let db = in_memory_database();
        let batch = OperationBatchRecord {
            batch_id: "batch-one".to_string(),
            request_id: "same-request".to_string(),
            kind: "fetch".to_string(),
            state: "queued".to_string(),
            message: "排队".to_string(),
            details_json: "[]".to_string(),
            ..OperationBatchRecord::default()
        };
        db.upsert_operation_batch(&batch).unwrap();

        let existing = db
            .operation_batch_by_request_id("same-request")
            .unwrap()
            .unwrap();

        assert_eq!(existing.batch_id, "batch-one");
    }

    #[test]
    fn fetch_batch_becomes_consumed_after_update_batch_is_created() {
        let db = in_memory_database();
        assert!(!db.fetch_batch_has_consumer("fetch-one").unwrap());

        db.upsert_operation_batch(&OperationBatchRecord {
            batch_id: "pull-one".to_string(),
            request_id: "request-pull-one".to_string(),
            kind: "pull-backup".to_string(),
            state: "queued".to_string(),
            message: "等待更新".to_string(),
            details_json: "[]".to_string(),
            source_batch_id: Some("fetch-one".to_string()),
            ..OperationBatchRecord::default()
        })
        .unwrap();

        assert!(db.fetch_batch_has_consumer("fetch-one").unwrap());
        assert!(!db.fetch_batch_has_consumer("fetch-two").unwrap());
    }
}
