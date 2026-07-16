//! 仓库同步检测模块
//!
//! 在拉取远程更新前检测磁盘与数据库的仓库路径差异，并按配置自动同步。
//!
//! # How it works
//!
//! 1. 快速遍历磁盘上的仓库路径集合
//! 2. 查询数据库中的仓库路径集合
//! 3. 比较集合差异；数量相同但路径不同仍会触发同步

use anyhow::Result;
use std::collections::HashSet;
use std::path::Path;
use walkdir::WalkDir;

use crate::db::Database;
use crate::models::ScanSource;
use crate::scanner::Scanner;

/// 同步检测结果
#[derive(Debug, Clone, PartialEq)]
pub enum SyncStatus {
    /// 路径集合一致，无需处理
    InSync { count: usize },
    /// 仅发现新增仓库
    NewReposFound {
        disk_count: usize,
        db_count: usize,
        new_count: usize,
    },
    /// 仅发现移除仓库
    ReposRemoved {
        disk_count: usize,
        db_count: usize,
        removed_count: usize,
    },
    /// 同时存在新增和移除仓库
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
                "仓库路径集合不一致（磁盘: {}，数据库: {}）",
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
        let disk_paths = self.collect_git_repo_paths(sources)?;

        // 数据库只保留当前扫描源负责的仓库，避免其他源影响判定。
        let db_repos = db.list_repositories()?;

        // Calculate difference
        let source_paths: HashSet<&str> = sources.iter().map(|s| s.root_path.as_str()).collect();
        let db_paths: HashSet<String> = db_repos
            .iter()
            .filter(|r| source_paths.contains(r.root_path.as_str()))
            .map(|repo| repo.path.clone())
            .collect();
        Ok(Self::compare_path_sets(&disk_paths, &db_paths))
    }

    /// 比较磁盘与数据库路径集合，避免“数量相同但仓库已替换”的漏检。
    fn compare_path_sets(disk_paths: &HashSet<String>, db_paths: &HashSet<String>) -> SyncStatus {
        let disk_count = disk_paths.len();
        let db_count = db_paths.len();
        let new_count = disk_paths.difference(db_paths).count();
        let removed_count = db_paths.difference(disk_paths).count();

        match (new_count, removed_count) {
            (0, 0) => SyncStatus::InSync { count: disk_count },
            (new_count, 0) => SyncStatus::NewReposFound {
                disk_count,
                db_count,
                new_count,
            },
            (0, removed_count) => SyncStatus::ReposRemoved {
                disk_count,
                db_count,
                removed_count,
            },
            _ => SyncStatus::Diverged {
                disk_count,
                db_count,
            },
        }
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
    #[cfg(test)]
    fn quick_count_git_dirs(&self, sources: &[ScanSource]) -> Result<usize> {
        Ok(self.collect_git_repo_paths(sources)?.len())
    }

    fn collect_git_repo_paths(&self, sources: &[ScanSource]) -> Result<HashSet<String>> {
        let mut paths = HashSet::new();

        for source in sources {
            if !source.enabled {
                continue;
            }

            let root = Path::new(&source.root_path);
            if !root.exists() {
                continue;
            }

            paths.extend(self.collect_git_dirs_in_source(root, source)?);
        }

        Ok(paths)
    }

    fn collect_git_dirs_in_source(
        &self,
        root: &Path,
        source: &ScanSource,
    ) -> Result<HashSet<String>> {
        let mut paths = HashSet::new();
        let max_depth = source.max_depth;

        let walker = WalkDir::new(root)
            // 与 Scanner 保持同一深度语义：仓库目录深度之外再读取 `.git` 标记层。
            .max_depth(max_depth.saturating_add(1))
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
            let entry = entry?;
            if entry.file_name() == ".git" && entry.file_type().is_dir() {
                let repo_path = entry.path().parent().unwrap_or(entry.path());
                let canonical = repo_path
                    .canonicalize()
                    .unwrap_or_else(|_| repo_path.to_path_buf());
                paths.insert(canonical.to_string_lossy().to_string());
            }
        }

        Ok(paths)
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

    #[test]
    fn equal_counts_with_replaced_repository_require_sync() {
        let disk_paths = HashSet::from(["/repos/kept".to_string(), "/repos/new".to_string()]);
        let db_paths = HashSet::from(["/repos/kept".to_string(), "/repos/removed".to_string()]);

        assert_eq!(
            RepoSync::compare_path_sets(&disk_paths, &db_paths),
            SyncStatus::Diverged {
                disk_count: 2,
                db_count: 2,
            }
        );
    }

    #[test]
    fn overlapping_sources_count_the_same_repository_once() {
        let tmp = TempDir::new().unwrap();
        let nested_root = tmp.path().join("nested");
        create_bare_git_repo(&nested_root.join("repo"));
        let source = |root: &Path| ScanSource {
            id: None,
            root_path: root.to_string_lossy().to_string(),
            max_depth: 5,
            ignore_patterns: vec![],
            follow_symlinks: false,
            enabled: true,
            last_scan_at: None,
        };

        let paths = RepoSync::new(true)
            .collect_git_repo_paths(&[source(tmp.path()), source(&nested_root)])
            .unwrap();

        assert_eq!(paths.len(), 1);
    }

    #[test]
    fn zero_depth_sync_detects_repository_at_source_root() -> anyhow::Result<()> {
        let temp = TempDir::new().unwrap();
        create_bare_git_repo(temp.path());
        let source = ScanSource {
            root_path: temp.path().to_string_lossy().to_string(),
            max_depth: 0,
            ignore_patterns: vec![],
            follow_symlinks: false,
            enabled: true,
            id: None,
            last_scan_at: None,
        };

        assert_eq!(RepoSync::new(true).quick_count_git_dirs(&[source])?, 1);
        Ok(())
    }
}
