//! TUI command handling.
//!
//! 命令层负责启动时全量同步、刷新本地 Git 状态、驱动交互式菜单，并在
//! 用户选择菜单项时继续执行修复或全量同步。

use anyhow::Result;
use colored::Colorize;
use std::path::PathBuf;

use crate::concurrent::execute_concurrent_raw;
use crate::db::Database;
use crate::git::{GitOps, ProxyConfig};
use crate::models::Repository;
use crate::workflow::{BuiltInWorkflows, WorkflowExecutor};

/// Execute the interactive repository console.
pub async fn execute(
    no_security_check: bool,
    auto_skip_high_risk: bool,
    proxy_config: Option<ProxyConfig>,
) -> Result<()> {
    let mut state = crate::tui::TuiState::default();
    let mut repos = refresh_repositories_for_tui_async().await?;
    if repos.is_empty() {
        println!("{} 暂无仓库记录，请先执行 scan 命令", "ℹ".blue());
        return Ok(());
    }

    repos =
        run_startup_full_sync(no_security_check, auto_skip_high_risk, proxy_config.clone()).await?;

    loop {
        if repos.is_empty() {
            println!("{} 暂无仓库记录，请先执行 scan 命令", "ℹ".blue());
            return Ok(());
        }

        crate::tui::render(&repos, &mut state)?;
        match crate::tui::read_action()? {
            crate::tui::TuiAction::Switch(tab) => state.switch_tab(tab),
            crate::tui::TuiAction::PrevPage => state.prev_page(),
            crate::tui::TuiAction::NextPage => {
                let total = crate::tui::current_tab_len(&repos, state.tab);
                state.next_page(total);
            }
            crate::tui::TuiAction::Refresh => {
                repos = refresh_repositories_for_tui_async().await?;
            }
            crate::tui::TuiAction::RunPullBackup => {
                let target_repos = crate::tui::pull_backup_targets(&repos);
                if target_repos.is_empty() {
                    crate::tui::wait_for_enter("当前没有需要 pull-backup 的异常仓库")?;
                    continue;
                }

                let target_counts = crate::tui::issue_counts(&target_repos);
                if crate::tui::confirm_pull_backup(target_counts)? {
                    let exit_code = run_pull_backup_workflow(
                        no_security_check,
                        auto_skip_high_risk,
                        proxy_config.clone(),
                        Some(target_repos),
                    )
                    .await?;
                    let message = if exit_code == 0 {
                        "异常修复已完成"
                    } else {
                        "异常修复已完成，但存在失败仓库"
                    };
                    repos = refresh_repositories_for_tui_async().await?;
                    crate::tui::wait_for_enter(message)?;
                } else {
                    crate::tui::wait_for_enter("已取消修复异常")?;
                }
            }
            crate::tui::TuiAction::RunFullPullBackup => {
                let counts = crate::tui::issue_counts(&repos);
                if crate::tui::confirm_full_pull_backup(repos.len(), counts)? {
                    let exit_code = run_pull_backup_workflow(
                        no_security_check,
                        auto_skip_high_risk,
                        proxy_config.clone(),
                        None,
                    )
                    .await?;
                    let message = if exit_code == 0 {
                        "全量同步已完成"
                    } else {
                        "全量同步已完成，但存在失败仓库"
                    };
                    repos = refresh_repositories_for_tui_async().await?;
                    crate::tui::wait_for_enter(message)?;
                } else {
                    crate::tui::wait_for_enter("已取消全量同步")?;
                }
            }
            crate::tui::TuiAction::Quit => return Ok(()),
            crate::tui::TuiAction::Ignore => {
                crate::tui::wait_for_enter("未识别的输入")?;
            }
        }
    }
}

async fn run_startup_full_sync(
    no_security_check: bool,
    auto_skip_high_risk: bool,
    proxy_config: Option<ProxyConfig>,
) -> Result<Vec<Repository>> {
    println!("{} 正在联网全量同步远程状态，请稍后...", "ℹ".blue());

    let exit_code =
        run_pull_backup_workflow(no_security_check, auto_skip_high_risk, proxy_config, None)
            .await?;
    if exit_code == 0 {
        println!("{} 启动全量同步完成，正在刷新结果...", "✓".green());
    } else {
        println!(
            "{} 启动全量同步完成，但存在失败仓库，正在刷新结果...",
            "⚠".yellow()
        );
    }

    refresh_repositories_for_tui_async().await
}

async fn refresh_repositories_for_tui_async() -> Result<Vec<Repository>> {
    println!("{} 正在读取本机仓库状态，请稍后...", "ℹ".blue());

    match tokio::time::timeout(
        std::time::Duration::from_secs(150),
        tokio::task::spawn_blocking(refresh_repositories_for_tui),
    )
    .await
    {
        Ok(Ok(Ok(repos))) => Ok(repos),
        Ok(Ok(Err(e))) => Err(e),
        Ok(Err(_)) => anyhow::bail!("TUI 本地刷新任务 panic"),
        Err(_) => anyhow::bail!("TUI 本地刷新超时（150s）"),
    }
}

async fn run_pull_backup_workflow(
    no_security_check: bool,
    auto_skip_high_risk: bool,
    proxy_config: Option<ProxyConfig>,
    target_repos: Option<Vec<Repository>>,
) -> Result<i32> {
    let workflow = BuiltInWorkflows::get("pull-backup")
        .ok_or_else(|| anyhow::anyhow!("内置 pull-backup 工作流不存在"))?;
    let mut executor = WorkflowExecutor::new(workflow, None, None, false, false)
        .with_security_check(!no_security_check)
        .with_auto_skip_high_risk(auto_skip_high_risk)
        .with_pull_safety_check(true);

    if let Some(repos) = target_repos {
        // `None` 表示沿用 workflow 的全库行为；`Some` 只由菜单 3 使用，
        // 确保“修复异常”和“全量同步”在命令层就有清晰边界。
        executor = executor.with_target_repositories(repos);
    }

    if let Some(proxy) = proxy_config
        && proxy.enabled
    {
        executor = executor.with_proxy(proxy);
    }

    let result = executor.execute().await?;
    Ok(result.exit_code())
}

/// Refresh local repository state before rendering TUI.
///
/// 这里刻意只执行 `GitOps::inspect()`，不执行 fetch：
/// - 本地修改、当前分支、工作区文件列表必须实时刷新，否则 TUI 会误导用户；
/// - 启动阶段的联网全量同步由 `run_startup_full_sync()` 统一负责。这里保持
///   为本地快照刷新函数，菜单 4 也能复用它而不产生额外联网副作用；
/// - 刷新结果串行写回 SQLite，下一次 `status --issues` 和 TUI 也能看到同一份本地快照。
fn refresh_repositories_for_tui() -> Result<Vec<Repository>> {
    let db = Database::open()?;
    let stored_repos = db.list_repositories()?;
    if stored_repos.is_empty() {
        return Ok(Vec::new());
    }

    let tasks: Vec<_> = stored_repos
        .into_iter()
        .map(|stored| {
            move || {
                let path = PathBuf::from(&stored.path);
                if !path.exists() {
                    return missing_repository_outcome(stored);
                }

                refresh_one_repository(stored)
            }
        })
        .collect();

    let results = execute_concurrent_raw(tasks, crate::utils::DEFAULT_MAX_CONCURRENT_SCAN);
    let mut repos = Vec::new();
    let mut stale_count = 0;

    for result in results {
        match result {
            Some(RefreshOutcome::Fresh(mut repo)) => {
                if let Err(e) = db.upsert_repository(&mut repo) {
                    eprintln!(
                        "{} TUI 刷新结果写入失败 {}: {}",
                        "⚠".yellow(),
                        crate::utils::sanitize_path(&repo.path),
                        e
                    );
                }
                repos.push(repo);
            }
            Some(RefreshOutcome::Stale { repo, reason }) => {
                stale_count += 1;
                eprintln!(
                    "{} TUI 保留旧快照 {}: {}",
                    "⚠".yellow(),
                    crate::utils::sanitize_path(&repo.path),
                    reason
                );
                repos.push(repo);
            }
            Some(RefreshOutcome::DeletedNeedauthOrphan { repo }) => {
                stale_count += 1;
                if let Err(e) = db.delete_repository(&repo.path) {
                    eprintln!(
                        "{} TUI 删除 needauth 孤儿记录失败 {}: {}",
                        "⚠".yellow(),
                        crate::utils::sanitize_path(&repo.path),
                        e
                    );
                    repos.push(repo);
                } else {
                    eprintln!(
                        "{} TUI 已删除不存在的 needauth 记录 {}",
                        "⚠".yellow(),
                        crate::utils::sanitize_path(&repo.path)
                    );
                }
            }
            None => {
                stale_count += 1;
                eprintln!("{} TUI 刷新任务 panic，已跳过一条记录", "⚠".yellow());
            }
        }
    }

    if stale_count > 0 {
        eprintln!(
            "{} TUI 有 {} 条记录无法本地刷新，界面中会保留旧快照",
            "⚠".yellow(),
            stale_count
        );
    }

    Ok(repos)
}

fn refresh_one_repository(stored: Repository) -> RefreshOutcome {
    let path = PathBuf::from(&stored.path);
    match GitOps::inspect(&path, &stored.root_path) {
        Ok(mut refreshed) => {
            // `inspect()` 只知道当前 Git 状态，不知道数据库记录 ID、最近 fetch
            // 和最近 pull 时间。这里把运行态元数据补回去，避免 TUI 刷新误清空
            // 用户判断远程信息新鲜度所依赖的时间戳。
            refreshed.id = stored.id;
            refreshed.last_fetch_at = stored.last_fetch_at;
            refreshed.last_pull_at = stored.last_pull_at;
            RefreshOutcome::Fresh(refreshed)
        }
        Err(e) => RefreshOutcome::Stale {
            repo: stored,
            reason: format!("重新检查失败：{e}"),
        },
    }
}

fn missing_repository_outcome(stored: Repository) -> RefreshOutcome {
    if stored.path.contains(crate::utils::NEEDAUTH_DIR) {
        return RefreshOutcome::DeletedNeedauthOrphan { repo: stored };
    }

    RefreshOutcome::Stale {
        repo: stored,
        reason: "路径不存在，保留数据库中的最后快照".to_string(),
    }
}

enum RefreshOutcome {
    Fresh(Repository),
    Stale { repo: Repository, reason: String },
    DeletedNeedauthOrphan { repo: Repository },
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    #[test]
    fn refresh_preserves_fetch_and_pull_timestamps() {
        let now = Local::now();
        let stored = Repository {
            id: Some(42),
            path: "/definitely/not/a/repo".to_string(),
            root_path: "/definitely/not".to_string(),
            last_fetch_at: Some(now),
            last_pull_at: Some(now),
            ..Repository::default()
        };

        let RefreshOutcome::Stale { repo, .. } = refresh_one_repository(stored) else {
            panic!("不存在的仓库应返回旧记录和错误原因");
        };

        assert_eq!(repo.id, Some(42));
        assert_eq!(repo.last_fetch_at, Some(now));
        assert_eq!(repo.last_pull_at, Some(now));
    }

    #[test]
    fn refresh_marks_missing_needauth_record_as_orphan() {
        let stored = Repository {
            path: format!(
                "/definitely/not/{}/missing-repo",
                crate::utils::NEEDAUTH_DIR
            ),
            root_path: format!("/definitely/not/{}", crate::utils::NEEDAUTH_DIR),
            ..Repository::default()
        };

        let RefreshOutcome::DeletedNeedauthOrphan { repo } = missing_repository_outcome(stored)
        else {
            panic!("不存在的 needauth 仓库应标记为可删除孤儿记录");
        };

        assert!(repo.path.contains(crate::utils::NEEDAUTH_DIR));
    }
}
