use anyhow::Result;
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
            if let Err(e) = db.upsert_repository(repo) {
                eprintln!(
                    "警告：保存仓库失败 '{}': {}",
                    crate::utils::sanitize_path(&repo.path),
                    e
                );
            }
        }

        // Clean up deleted repository records
        Self::cleanup_deleted_repos(db, &source.root_path, &repos)?;

        Ok(repos)
    }

    /// Find all Git repository directories
    fn find_git_dirs(root: &Path, source: &ScanSource) -> Result<Vec<std::path::PathBuf>> {
        let mut git_dirs = Vec::new();
        let max_depth = source.max_depth;

        let walker = WalkDir::new(root)
            .max_depth(max_depth)
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
            match entry {
                Ok(e) => {
                    // Check for .git directory
                    if e.file_name() == ".git" && e.file_type().is_dir() {
                        git_dirs.push(e.path().to_path_buf());
                    }
                }
                Err(e) => {
                    // Log WalkDir errors but don't interrupt the scan
                    if let Some(path) = e.path() {
                        eprintln!("   警告：无法访问路径 '{}': {}", path.display(), e);
                    } else {
                        eprintln!("   警告：扫描错误: {}", e);
                    }
                }
            }
        }

        Ok(git_dirs)
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
        current_repos: &[Repository],
    ) -> Result<()> {
        // Get all records under this root_path
        let existing = db.list_repositories()?;
        let current_paths: std::collections::HashSet<String> =
            current_repos.iter().map(|r| r.path.clone()).collect();

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

            if repo.root_path == root_path && !current_paths.contains(&repo.path) {
                // Before deleting, check if the repository was moved to needauth/
                let needauth_path = std::path::Path::new(root_path)
                    .join(crate::utils::NEEDAUTH_DIR)
                    .join(&repo.name);
                if needauth_path.exists() {
                    // Update record to new path instead of deleting
                    let mut updated = repo;
                    updated.path = needauth_path.to_string_lossy().to_string();
                    updated.root_path = std::path::Path::new(root_path)
                        .join(crate::utils::NEEDAUTH_DIR)
                        .to_string_lossy()
                        .to_string();
                    if let Err(e) = db.upsert_repository(&mut updated) {
                        eprintln!("警告：更新已移动仓库记录失败 '{}': {}", updated.name, e);
                    }
                } else if let Some(moved_path) =
                    Self::find_moved_repo_in_needauth(root_path, &repo.path)
                {
                    // 仓库被重命名后移动到 needauth，通过 sidecar 文件定位
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
                    if let Err(e) = db.upsert_repository(&mut updated) {
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
                }
            }
        }

        Ok(all_repos)
    }
}
