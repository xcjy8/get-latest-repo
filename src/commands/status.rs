//! Status command handling

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::PathBuf;

use crate::db::Database;
use crate::git::GitOps;
use crate::models::Repository;
use crate::reporter::terminal::{print_issues_view, print_repo_detail};

/// Execute status command
pub async fn execute(path: Option<PathBuf>, show_diff: bool, issues: bool) -> Result<()> {
    if issues {
        let db = Database::open()?;
        let repos = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::task::spawn_blocking(move || db.list_repositories()),
        )
        .await
        {
            Ok(Ok(Ok(repos))) => repos,
            Ok(Ok(Err(e))) => return Err(e),
            Ok(Err(_)) => anyhow::bail!("数据库查询任务 panic"),
            Err(_) => anyhow::bail!("数据库查询超时（30s）"),
        };
        if repos.is_empty() {
            println!("{} 暂无仓库记录，请先执行 scan 命令", "ℹ".blue());
            return Ok(());
        }
        print_issues_view(&repos);
        return Ok(());
    }

    let path =
        path.ok_or_else(|| anyhow::anyhow!("请提供仓库路径，或使用 --issues 查看所有异常仓库"))?;

    let canonical = path
        .canonicalize()
        .with_context(|| format!("无法访问路径：{}", path.display()))?;

    if !GitOps::is_repository(&canonical) {
        anyhow::bail!("不是有效的 Git 仓库：{}", canonical.display());
    }

    let inspect_path = canonical.clone();
    let repo = tokio::task::spawn_blocking(move || -> Result<Repository> {
        let db = Database::open()?;
        let path_text = inspect_path.to_string_lossy().to_string();
        let cached = db.get_repository(&path_text)?;
        let root_path = cached
            .as_ref()
            .map(|repo| repo.root_path.clone())
            .or_else(|| {
                inspect_path
                    .parent()
                    .map(|parent| parent.to_string_lossy().to_string())
            })
            .unwrap_or_default();
        let current = GitOps::inspect(&inspect_path, &root_path)?;
        let mut refreshed = merge_repository_history(current, cached.as_ref());
        db.upsert_repository(&mut refreshed)?;
        Ok(refreshed)
    })
    .await
    .map_err(|error| anyhow::anyhow!("仓库状态检查任务异常：{error}"))??;

    print_repo_detail(&repo);

    if show_diff && repo.dirty {
        println!("\n{} 本地已修改文件：", "📝".yellow());
        for file in &repo.dirty_files {
            println!("  - {}", file);
        }
    }

    Ok(())
}

/// 将实时检查结果与数据库历史字段合并，避免刷新状态时丢失操作时间。
fn merge_repository_history(mut current: Repository, cached: Option<&Repository>) -> Repository {
    if let Some(cached) = cached {
        current.id = cached.id;
        current.last_fetch_at = cached.last_fetch_at;
        current.last_pull_at = cached.last_pull_at;
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    #[test]
    fn realtime_status_preserves_database_history() {
        let fetch_at = Local::now();
        let pull_at = fetch_at - chrono::Duration::minutes(1);
        let cached = Repository {
            id: Some(42),
            dirty: false,
            last_fetch_at: Some(fetch_at),
            last_pull_at: Some(pull_at),
            ..Repository::default()
        };
        let current = Repository {
            dirty: true,
            last_fetch_at: None,
            last_pull_at: None,
            ..Repository::default()
        };

        let merged = merge_repository_history(current, Some(&cached));

        assert!(merged.dirty, "必须使用实时检查得到的工作区状态");
        assert_eq!(merged.id, Some(42));
        assert_eq!(merged.last_fetch_at, Some(fetch_at));
        assert_eq!(merged.last_pull_at, Some(pull_at));
    }
}
