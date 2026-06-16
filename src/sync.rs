//! Repository sync detection module
//!
//! Provides intelligent detection mechanism, automatically discovers and syncs newly cloned repositories before fetch.
//!
//! # How it works
//!
//! 1. Quick count: traverse scan source directories, count `.git` directories (don't read contents, O(n) complexity)
//! 2. Database query: get current recorded repository count
//! 3. Comparison decision:
//!    - Disk count > DB count: new repositories, trigger scan
//!    - Disk count < DB count: repositories deleted, trigger cleanup
//!    - Equal: skip scan, fetch directly

use anyhow::Result;
use std::collections::HashSet;
use std::path::Path;
use walkdir::WalkDir;

use crate::db::Database;
use crate::models::ScanSource;
use crate::scanner::Scanner;

/// Sync detection results
#[derive(Debug, Clone, PartialEq)]
pub enum SyncStatus {
    /// Synced, no action needed
    InSync { count: usize },
    /// New repositories found
    NewReposFound {
        disk_count: usize,
        db_count: usize,
        new_count: usize,
    },
    /// Repositories were deleted
    ReposRemoved {
        disk_count: usize,
        db_count: usize,
        removed_count: usize,
    },
    /// Both added and removed
    #[allow(dead_code)]
    Diverged { disk_count: usize, db_count: usize },
}

impl SyncStatus {
    /// Whether full scan is needed
    pub fn needs_scan(&self) -> bool {
        !matches!(self, SyncStatus::InSync { .. })
    }

    /// Get human-readable description
    pub fn description(&self) -> String {
        match self {
            SyncStatus::InSync { count } => format!("已同步（{} 个仓库）", count),
            SyncStatus::NewReposFound {
                disk_count,
                db_count,
                new_count,
            } => format!(
                "发现 {} 个新增仓库（磁盘: {}，数据库: {}）",
                new_count, disk_count, db_count
            ),
            SyncStatus::ReposRemoved {
                disk_count,
                db_count,
                removed_count,
            } => format!(
                "{} 个仓库已删除（磁盘: {}，数据库: {}）",
                removed_count, disk_count, db_count
            ),
            SyncStatus::Diverged {
                disk_count,
                db_count,
            } => format!(
                "仓库数量不一致（磁盘: {}，数据库: {}）",
                disk_count, db_count
            ),
        }
    }
}

/// Repository syncer
pub struct RepoSync {
    /// Whether auto-sync is enabled
    auto_sync: bool,
}

impl RepoSync {
    /// Create new syncer
    pub fn new(auto_sync: bool) -> Self {
        Self { auto_sync }
    }

    /// Check repository sync status
    ///
    /// # Parameters
    /// - `sources`: Scan source list
    /// - `db`: database connection
    ///
    /// # Returns
    /// - `Ok(SyncStatus)`: Sync status
    /// - `Err`: Traversal or database error
    pub fn check_sync_status(&self, sources: &[ScanSource], db: &Database) -> Result<SyncStatus> {
        // Quickly count repositories on disk
        let disk_count = self.quick_count_git_dirs(sources)?;

        // Get repository count from database
        let db_repos = db.list_repositories()?;
        let _db_count = db_repos.len();

        // Calculate difference
        let source_paths: HashSet<&str> = sources.iter().map(|s| s.root_path.as_str()).collect();
        let relevant_db_repos: Vec<_> = db_repos
            .iter()
            .filter(|r| source_paths.contains(r.root_path.as_str()))
            .collect();
        let db_count = relevant_db_repos.len();

        // Compare and return status
        Ok(match disk_count.cmp(&db_count) {
            std::cmp::Ordering::Equal => SyncStatus::InSync { count: disk_count },
            std::cmp::Ordering::Greater => SyncStatus::NewReposFound {
                disk_count,
                db_count,
                new_count: disk_count - db_count,
            },
            std::cmp::Ordering::Less => SyncStatus::ReposRemoved {
                disk_count,
                db_count,
                removed_count: db_count - disk_count,
            },
        })
    }

    /// Ensure repositories are synced
    ///
    /// If out of sync detected, automatically execute scan.
    ///
    /// # Parameters
    /// - `sources`: Scan source list
    /// - `db`: database connection
    /// - `progress`: Whether to show progress
    ///
    /// # Returns
    /// - `Ok(SyncStatus)`: Sync status after execution (should be InSync)
    pub async fn ensure_synced(
        &self,
        sources: &[ScanSource],
        db: &Database,
        progress: bool,
    ) -> Result<SyncStatus> {
        if !self.auto_sync {
            // Auto-sync disabled, return current status directly
            let status = self.check_sync_status(sources, db)?;
            return Ok(status);
        }

        let status = self.check_sync_status(sources, db)?;

        if status.needs_scan() {
            if progress {
                println!("📁 {}，正在同步...", status.description());
            }

            // Execute full scan
            Scanner::scan_all(
                sources,
                db,
                progress,
                crate::utils::DEFAULT_MAX_CONCURRENT_SCAN,
            )
            .await?;

            // Recheck to confirm synced
            let final_status = self.check_sync_status(sources, db)?;
            Ok(final_status)
        } else {
            Ok(status)
        }
    }

    /// Quickly count Git repositories
    ///
    /// Only traverses the directory structure; does not open or read repository contents. Minimal performance overhead.
    ///
    /// # Parameters
    /// - `sources`: Scan source list
    ///
    /// # Returns
    /// - `Ok(usize)`: Number of `.git` directories found
    fn quick_count_git_dirs(&self, sources: &[ScanSource]) -> Result<usize> {
        let mut total_count = 0;

        for source in sources {
            if !source.enabled {
                continue;
            }

            let root = Path::new(&source.root_path);
            if !root.exists() {
                continue;
            }

            let count = self.count_git_dirs_in_source(root, source)?;
            total_count += count;
        }

        Ok(total_count)
    }

    /// Count Git repositories in single source directory
    fn count_git_dirs_in_source(&self, root: &Path, source: &ScanSource) -> Result<usize> {
        let mut count = 0;
        let max_depth = source.max_depth;

        let walker = WalkDir::new(root)
            .max_depth(max_depth)
            .follow_links(source.follow_symlinks)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                // Skip ignored directories
                if name == ".git" {
                    return true;
                }
                // 跳过 needauth 目录，避免已移动的仓库被重复计数
                if name == crate::utils::NEEDAUTH_DIR {
                    return false;
                }
                !crate::utils::should_ignore_entry(&name, &source.ignore_patterns)
            });

        for entry in walker {
            match entry {
                Ok(e) => {
                    if e.file_name() == ".git" && e.file_type().is_dir() {
                        count += 1;
                    }
                }
                Err(_) => {
                    // Ignore walkdir errors, continue counting
                    continue;
                }
            }
        }

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: Create minimal git repository
    fn create_bare_git_repo(path: &Path) {
        fs::create_dir_all(path.join(".git")).unwrap();
        // Create most basic git structure for WalkDir recognition
        fs::write(path.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    #[test]
    fn test_sync_status_in_sync() {
        let status = SyncStatus::InSync { count: 10 };
        assert!(!status.needs_scan());
        assert!(status.description().contains("已同步"));
    }

    #[test]
    fn test_sync_status_new_repos() {
        let status = SyncStatus::NewReposFound {
            disk_count: 12,
            db_count: 10,
            new_count: 2,
        };
        assert!(status.needs_scan());
        assert!(status.description().contains("发现"));
        assert!(status.description().contains("2"));
    }

    #[test]
    fn test_sync_status_repos_removed() {
        let status = SyncStatus::ReposRemoved {
            disk_count: 8,
            db_count: 10,
            removed_count: 2,
        };
        assert!(status.needs_scan());
        assert!(status.description().contains("已删除"));
    }

    #[test]
    fn test_quick_count_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let source = ScanSource {
            id: None,
            root_path: tmp.path().to_string_lossy().to_string(),
            max_depth: 5,
            ignore_patterns: vec![],
            follow_symlinks: false,
            enabled: true,
            last_scan_at: None,
        };

        let sync = RepoSync::new(true);
        let count = sync.quick_count_git_dirs(&[source]).unwrap();

        assert_eq!(count, 0);
    }

    #[test]
    fn test_quick_count_multiple_repos() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Create 3 git repositories
        create_bare_git_repo(&root.join("repo1"));
        create_bare_git_repo(&root.join("repo2"));
        create_bare_git_repo(&root.join("nested/repo3"));

        let source = ScanSource {
            id: None,
            root_path: root.to_string_lossy().to_string(),
            max_depth: 5,
            ignore_patterns: vec!["node_modules".to_string()],
            follow_symlinks: false,
            enabled: true,
            last_scan_at: None,
        };

        let sync = RepoSync::new(true);
        let count = sync.quick_count_git_dirs(&[source]).unwrap();

        assert_eq!(count, 3);
    }

    #[test]
    fn test_quick_count_respects_max_depth() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Create repository at depth 2
        create_bare_git_repo(&root.join("a/repo1"));
        // Depth 4 (exceeds max_depth=3)
        create_bare_git_repo(&root.join("a/b/c/repo2"));

        let source = ScanSource {
            id: None,
            root_path: root.to_string_lossy().to_string(),
            max_depth: 3,
            ignore_patterns: vec![],
            follow_symlinks: false,
            enabled: true,
            last_scan_at: None,
        };

        let sync = RepoSync::new(true);
        let count = sync.quick_count_git_dirs(&[source]).unwrap();

        // Should only count repo1 at depth 3
        assert_eq!(count, 1);
    }

    #[test]
    fn test_quick_count_ignores_disabled_sources() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        create_bare_git_repo(&root.join("repo1"));

        let source = ScanSource {
            id: None,
            root_path: root.to_string_lossy().to_string(),
            max_depth: 5,
            ignore_patterns: vec![],
            follow_symlinks: false,
            enabled: false, // disabled
            last_scan_at: None,
        };

        let sync = RepoSync::new(true);
        let count = sync.quick_count_git_dirs(&[source]).unwrap();

        assert_eq!(count, 0);
    }

    #[test]
    fn test_check_sync_status_detects_new() {
        // This test needs database, use integration test approach
        // Simplified version: only test status calculation logic
        let status = SyncStatus::NewReposFound {
            disk_count: 15,
            db_count: 10,
            new_count: 5,
        };

        assert!(status.needs_scan());
        assert_eq!(
            status.description(),
            "发现 5 个新增仓库（磁盘: 15，数据库: 10）"
        );
    }

    #[test]
    fn test_repo_sync_preserves_auto_sync_setting() {
        assert!(RepoSync::new(true).auto_sync);
        assert!(!RepoSync::new(false).auto_sync);
    }
}
