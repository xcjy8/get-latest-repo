//! Discard command - discard local changes
//!
//! Allows users to discard all local changes in a specified repository, then continue fetching or pulling

use anyhow::Result;
use colored::Colorize;
use std::io::Write;

use crate::db::Database;
use crate::git::GitOps;

/// Execute discard command
pub async fn execute(repo_path: Option<String>, yes: bool) -> Result<()> {
    let db = Database::open()?;

    // Determine target repository
    let target_path = match repo_path {
        Some(path) => path,
        None => {
            // If no path specified, show all repositories with local changes for user selection
            let repos = db.list_repositories()?;
            let dirty_repos: Vec<_> = repos.into_iter().filter(|r| r.dirty).collect();

            if dirty_repos.is_empty() {
                println!("{} 未发现有本地修改的仓库", "ℹ".blue());
                return Ok(());
            }

            println!(
                "{} 发现 {} 个有本地修改的仓库：",
                "📋".cyan(),
                dirty_repos.len()
            );
            println!();

            for (i, repo) in dirty_repos.iter().enumerate() {
                let branch_info = repo.branch.as_deref().unwrap_or("未知");
                println!(
                    "  [{}] {} [{}]（{} 个文件）",
                    i + 1,
                    repo.name.bold(),
                    branch_info.dimmed(),
                    repo.dirty_files.len()
                );
                // Show first few changed files
                for file in repo.dirty_files.iter().take(3) {
                    println!("      - {}", file.dimmed());
                }
                if repo.dirty_files.len() > 3 {
                    println!("      ... 另有 {} 个文件", repo.dirty_files.len() - 3);
                }
                println!();
            }

            print!(
                "请选择要丢弃修改的仓库编号（1-{}），输入 0 取消：",
                dirty_repos.len()
            );
            std::io::stdout().flush()?;

            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;

            let choice: usize = input
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("输入无效"))?;

            if choice == 0 || choice > dirty_repos.len() {
                println!("{} 已取消", "✓".green());
                return Ok(());
            }

            dirty_repos[choice - 1].path.clone()
        }
    };

    // Validate path
    let path = std::path::PathBuf::from(&target_path);
    if !path.exists() {
        anyhow::bail!("路径不存在：{}", target_path);
    }

    // Get repository info for display
    let repo_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| target_path.clone());

    // Confirmation prompt
    if !yes {
        println!();
        println!("{} 警告：此操作会永久丢弃所有本地修改！", "⚠️".red().bold());
        println!();
        println!("  仓库：{}", repo_name.bold());
        println!("  路径：{}", target_path.dimmed());
        println!();
        println!("  将丢弃的内容包括：");
        println!("    - 工作区所有修改");
        println!("    - 暂存区所有修改");
        println!("    - 未跟踪文件");
        println!();
        print!("{} 确认丢弃这些修改？[y/N] ", "❓".yellow());
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;

        if !input.trim().eq_ignore_ascii_case("y") {
            println!("{} 已取消", "✓".green());
            return Ok(());
        }
    }

    // Execute discard operation
    println!();
    println!("{} 正在丢弃 {} 的本地修改...", "🗑️".yellow(), repo_name);

    match GitOps::discard_changes(&path, true) {
        Ok(discarded_files) => {
            println!(
                "{} 已丢弃 {} 个文件的修改",
                "✓".green(),
                discarded_files.len()
            );

            if !discarded_files.is_empty() {
                println!();
                println!("{} 已丢弃文件：", "📄".dimmed());
                for (i, file) in discarded_files.iter().take(10).enumerate() {
                    println!("  {} {}", "-".dimmed(), file.dimmed());
                    if i == 9 && discarded_files.len() > 10 {
                        println!("  ... 另有 {} 个文件", discarded_files.len() - 10);
                        break;
                    }
                }
            }

            // Update repository status in database
            if let Ok(Some(mut repo)) = db.get_repository(&target_path) {
                repo.dirty = false;
                repo.dirty_files.clear();
                repo.file_changes.clear();
                if let Err(e) = db.upsert_repository(&mut repo) {
                    eprintln!("{} 更新数据库状态失败：{}", "⚠️".yellow(), e);
                }
            }

            println!();
            println!(
                "{} 现在可以运行 'getlatestrepo fetch' 或 'getlatestrepo workflow pull-safe'",
                "💡".cyan()
            );
        }
        Err(e) => {
            anyhow::bail!("{} 丢弃修改失败：{}", "✗".red(), e);
        }
    }

    Ok(())
}
