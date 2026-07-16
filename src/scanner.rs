use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use std::sync::{Arc, Mutex};
use walkdir::WalkDir;

use crate::concurrent::execute_concurrent_raw;
use crate::db::Database;
use crate::git::GitOps;
use crate::models::{Repository, ScanSource};

/// Repository scanner
pub struct Scanner;

impl Scanner {
    /// Scan single source directory (concurrent inspect)
    pub async fn scan_source(
        source: &ScanSource,
        db: &Database,
        progress: bool,
        jobs: usize,
    ) -> Result<Vec<Repository>> {
        let root = Path::new(&source.root_path);

        if !root.exists() {
            anyhow::bail!("扫描路径不存在: {}", source.root_path);
        }

        // Find all .git directories (blocking IO, run in dedicated thread)
        let root_buf = root.to_path_buf();
        let source_clone = source.clone();
        let git_dirs =
            tokio::task::spawn_blocking(move || Self::find_git_dirs(&root_buf, &source_clone))
                .await??;
        // 发现集合与 inspect 成功集合必须分离：仓库即使暂时无法打开，也仍然存在于磁盘。
        let discovered_paths = Self::repo_paths_from_git_dirs(&git_dirs);

        let pb: Option<Arc<Mutex<ProgressBar>>> = if progress {
            let bar = ProgressBar::new(git_dirs.len() as u64);
            bar.set_style(
                ProgressStyle::default_bar()
                    .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}")?
                    .progress_chars("#>-"),
            );
            Some(Arc::new(Mutex::new(bar)))
        } else {
            None
        };

        // ── Concurrent inspect ──────────────────────────────────────────────
        // Use unified concurrent executor, solving the following problems:
        // - Auto-handle panics (won't cause hung)
        // - Uses blocking wait (no busy-wait)
        // - Reasonable timeout (5 seconds)
        let max_concurrent = jobs.clamp(1, 100);

        // Build task list
        let tasks: Vec<_> = git_dirs
            .into_iter()
            .map(|git_dir| {
                let repo_path = git_dir.parent().unwrap_or(&git_dir).to_path_buf();
                let root_path = source.root_path.clone();
                let pb = pb.clone();

                move || {
                    let repo_name = repo_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();

                    if let Some(ref bar) = pb
                        && let Ok(bar) = bar.lock()
                    {
                        bar.set_message(format!("扫描 {}", repo_name));
                    }

                    let result = GitOps::inspect(&repo_path, &root_path);

                    if let Some(ref bar) = pb
                        && let Ok(bar) = bar.lock()
                    {
                        bar.inc(1);
                    }

                    result.map_err(|e| e.to_string())
                }
            })
            .collect();

        // Execute concurrent tasks
        let results = execute_concurrent_raw(tasks, max_concurrent);

        let mut repos = Vec::new();
        let mut errors = Vec::new();

        for result in results {
            match result {
                Some(Ok(repo)) => repos.push(repo),
                Some(Err(e)) => errors.push(e),
                None => errors.push("扫描任务 panic".to_string()),
            }
        }

        if let Some(ref bar) = pb
            && let Ok(bar) = bar.lock()
        {
            bar.finish_with_message("扫描完成");
        }

        // Display errors
        for err in &errors {
            eprintln!("⚠ {}", err);
        }

        // Batch write to the database serially to ensure SQLite consistency
        for repo in &mut repos {
            db.upsert_repository(repo).with_context(|| {
                format!("保存仓库失败：{}", crate::utils::sanitize_path(&repo.path))
            })?;
        }

        // Clean up deleted repository records
        Self::cleanup_deleted_repos(db, &source.root_path, &discovered_paths)?;

        Ok(repos)
    }

    /// Find all Git repository directories
    fn find_git_dirs(root: &Path, source: &ScanSource) -> Result<Vec<std::path::PathBuf>> {
        let mut git_dirs = Vec::new();
        let max_depth = source.max_depth;

        let walker = WalkDir::new(root)
            // max_depth 描述仓库目录相对扫描根的深度；识别标记 `.git` 还需多走一层。
            .max_depth(max_depth.saturating_add(1))
            .follow_links(source.follow_symlinks)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                // Skip ignored directories (but keep .git for detection)
                if name == ".git" {
                    return true; // Keep .git directory for detection
                }
                // 跳过 needauth 目录，避免已移动仓库被重复扫描入库
                if name == crate::utils::NEEDAUTH_DIR {
                    return false;
                }
                !crate::utils::should_ignore_entry(&name, &source.ignore_patterns)
            });

        for entry in walker {
            // 目录遍历不完整时不能继续做删除对账，否则不可访问区域中的正常仓库会被误删。
            let entry = entry?;
            if entry.file_name() == ".git" && entry.file_type().is_dir() {
                git_dirs.push(entry.path().to_path_buf());
            }
        }

        Ok(git_dirs)
    }

    /// 把发现阶段的 `.git` 路径转换为仓库路径，供删除对账使用。
    ///
    /// 该集合不依赖后续 inspect 是否成功，从而保留暂时损坏、被占用或权限受限的仓库记录。
    fn repo_paths_from_git_dirs(
        git_dirs: &[std::path::PathBuf],
    ) -> std::collections::HashSet<String> {
        git_dirs
            .iter()
            .map(|git_dir| {
                git_dir
                    .parent()
                    .unwrap_or(git_dir)
                    .to_string_lossy()
                    .to_string()
            })
            .collect()
    }

    /// 在 needauth 目录下查找从指定路径移动过来的仓库（支持重命名）
    ///
    /// 通过读取 `.needauth_original_path` sidecar 文件来匹配原始相对路径。
    fn find_moved_repo_in_needauth(
        root_path: &str,
        original_path: &str,
    ) -> Option<std::path::PathBuf> {
        let needauth_dir = std::path::Path::new(root_path).join(crate::utils::NEEDAUTH_DIR);
        if !needauth_dir.exists() {
            return None;
        }

        let original_relative = std::path::Path::new(original_path)
            .strip_prefix(root_path)
            .unwrap_or(std::path::Path::new(original_path));

        if let Ok(entries) = std::fs::read_dir(&needauth_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let sidecar = path.join(".needauth_original_path");
                    if let Ok(content) = std::fs::read_to_string(&sidecar) {
                        let content = content.trim();
                        if content == original_relative.to_string_lossy() {
                            return Some(path);
                        }
                    }
                }
            }
        }

        None
    }

    /// Clean up repository records that no longer exist in the database
    fn cleanup_deleted_repos(
        db: &Database,
        root_path: &str,
        discovered_paths: &std::collections::HashSet<String>,
    ) -> Result<()> {
        // Get all records under this root_path
        let existing = db.list_repositories()?;
        for repo in existing {
            // 清理 needauth 孤儿记录：手动删除 needauth 目录后，DB 中指向不存在的 needauth 路径的记录
            // NOTE: Blocking filesystem I/O. Acceptable here because this runs in a blocking context.
            let expected_needauth_root =
                std::path::Path::new(root_path).join(crate::utils::NEEDAUTH_DIR);
            if repo.root_path == expected_needauth_root.to_string_lossy()
                && !std::path::Path::new(&repo.path).exists()
            {
                if let Err(e) = db.delete_repository(&repo.path) {
                    eprintln!("警告：删除 needauth 孤儿记录失败 '{}': {}", repo.name, e);
                }
                continue;
            }

            if repo.root_path == root_path && !discovered_paths.contains(&repo.path) {
                // Before deleting, check if the repository was moved to needauth/
                let needauth_path = std::path::Path::new(root_path)
                    .join(crate::utils::NEEDAUTH_DIR)
                    .join(&repo.name);
                if needauth_path.exists() {
                    // 原路径删除与新路径写入必须处于同一事务，崩溃后不能留下双记录。
                    let old_path = repo.path.clone();
                    let mut updated = repo;
                    updated.path = needauth_path.to_string_lossy().to_string();
                    updated.root_path = std::path::Path::new(root_path)
                        .join(crate::utils::NEEDAUTH_DIR)
                        .to_string_lossy()
                        .to_string();
                    if let Err(e) = db.move_repository(&old_path, &mut updated) {
                        eprintln!("警告：更新已移动仓库记录失败 '{}': {}", updated.name, e);
                    }
                } else if let Some(moved_path) =
                    Self::find_moved_repo_in_needauth(root_path, &repo.path)
                {
                    // 仓库被重命名后移动到 needauth，通过 sidecar 文件定位。
                    let old_path = repo.path.clone();
                    let mut updated = repo;
                    updated.path = moved_path.to_string_lossy().to_string();
                    updated.root_path = std::path::Path::new(root_path)
                        .join(crate::utils::NEEDAUTH_DIR)
                        .to_string_lossy()
                        .to_string();
                    updated.name = moved_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| updated.name.clone());
                    if let Err(e) = db.move_repository(&old_path, &mut updated) {
                        eprintln!("警告：更新已移动仓库记录失败 '{}': {}", updated.name, e);
                    }
                } else {
                    db.delete_repository(&repo.path)?;
                }
            }
        }

        Ok(())
    }

    /// Scan all configured sources
    pub async fn scan_all(
        sources: &[ScanSource],
        db: &Database,
        progress: bool,
        jobs: usize,
    ) -> Result<Vec<Repository>> {
        let mut all_repos = Vec::new();
        let mut source_errors = Vec::new();

        for source in sources {
            if !source.enabled {
                continue;
            }

            if progress {
                println!("\n📁 扫描: {}", source.root_path);
            }

            match Self::scan_source(source, db, progress, jobs).await {
                Ok(mut repos) => {
                    all_repos.append(&mut repos);
                }
                Err(e) => {
                    eprintln!("❌ 扫描失败 {}: {}", source.root_path, e);
                    source_errors.push(format!("{}：{}", source.root_path, e));
                }
            }
        }

        if source_errors.is_empty() {
            Ok(all_repos)
        } else {
            anyhow::bail!(
                "扫描未完整成功（{} 个扫描源失败）：{}",
                source_errors.len(),
                source_errors.join("；")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovered_repo_remains_present_even_when_inspect_is_not_run() {
        let repo_path = std::path::PathBuf::from("/tmp/repo-with-temporary-inspect-error");
        let git_dirs = vec![repo_path.join(".git")];

        let discovered = Scanner::repo_paths_from_git_dirs(&git_dirs);

        assert!(discovered.contains(&repo_path.to_string_lossy().to_string()));
    }

    #[test]
    fn needauth_recovery_moves_database_record_without_duplication() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let old_path = root.join("repo");
        let moved_path = root.join(crate::utils::NEEDAUTH_DIR).join("repo");
        std::fs::create_dir_all(&moved_path).unwrap();
        let db = Database::open_in_memory_for_test();
        let mut repo = Repository {
            path: old_path.to_string_lossy().to_string(),
            root_path: root.to_string_lossy().to_string(),
            name: "repo".to_string(),
            ..Repository::default()
        };
        db.upsert_repository(&mut repo).unwrap();

        Scanner::cleanup_deleted_repos(
            &db,
            &root.to_string_lossy(),
            &std::collections::HashSet::new(),
        )
        .unwrap();

        assert!(
            db.get_repository(&old_path.to_string_lossy())
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_repository(&moved_path.to_string_lossy())
                .unwrap()
                .is_some()
        );
        assert_eq!(db.list_repositories().unwrap().len(), 1);
    }

    #[test]
    fn zero_depth_includes_repository_at_scan_root() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        let source = ScanSource {
            root_path: temp.path().to_string_lossy().to_string(),
            max_depth: 0,
            ignore_patterns: vec![],
            ..ScanSource::default()
        };

        let git_dirs = Scanner::find_git_dirs(temp.path(), &source).unwrap();

        assert_eq!(git_dirs, vec![temp.path().join(".git")]);
    }

    #[tokio::test]
    async fn scan_all_returns_error_when_any_source_fails() {
        let db = Database::open_in_memory_for_test();
        let source = ScanSource {
            root_path: "/path/that/does/not/exist/getlatestrepo".to_string(),
            enabled: true,
            ..ScanSource::default()
        };

        let result = Scanner::scan_all(&[source], &db, false, 1).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("扫描未完整成功"));
    }
}
