use anyhow::Result;
use colored::*;
use comfy_table::{ContentArrangement, Table, modifiers::UTF8_ROUND_CORNERS};

use super::Reporter;
use crate::git::format_duration;
use crate::models::{Freshness, RepoSummary, Repository};

pub struct TerminalReporter;

impl TerminalReporter {
    pub fn new() -> Self {
        Self
    }
}

impl Reporter for TerminalReporter {
    fn generate(&self, repos: &[Repository], summary: &RepoSummary) -> Result<String> {
        let mut output = String::new();

        // Summary
        output.push_str(&format!("\n{}\n", "📊 摘要".bold().underline()));
        output.push_str(&format!(
            "   总数: {} | {} 需要更新 | {} 已同步 | {} 有本地修改 | {} 远程不可达\n",
            summary.total.to_string().cyan(),
            format!("{}", summary.has_updates).red().bold(),
            format!("{}", summary.synced).green(),
            format!("{}", summary.dirty).yellow(),
            format!("{}", summary.unreachable).dimmed()
        ));

        // Compact mode for large repo counts: only show repos that need attention
        if repos.len() >= 20 {
            let attention_repos: Vec<&Repository> = repos
                .iter()
                .filter(|r| r.behind_count > 0 || r.dirty || r.freshness == Freshness::Unreachable)
                .collect();

            if attention_repos.is_empty() {
                output.push_str(&format!("\n   {} 所有仓库均已同步\n", "✓".green()));
            } else {
                output.push_str(&format!("\n{}\n", "⚠️  需要关注的仓库：".bold()));

                let mut table = Table::new();
                table
                    .set_content_arrangement(ContentArrangement::Dynamic)
                    .apply_modifier(UTF8_ROUND_CORNERS)
                    .set_header(vec![
                        "仓库".cell(),
                        "分支".cell(),
                        "状态".cell(),
                        "详情".cell(),
                    ]);

                for repo in &attention_repos {
                    let status = if repo.freshness == Freshness::Unreachable {
                        "⚫ 远程不可达".dimmed().to_string()
                    } else if repo.behind_count > 0 && repo.dirty {
                        "🔴 落后 + 本地修改".to_string()
                    } else if repo.behind_count > 0 {
                        format!("🔴 落后 {} 个提交", repo.behind_count)
                    } else {
                        "📝 本地修改".yellow().to_string()
                    };

                    let detail = if repo.freshness == Freshness::Unreachable {
                        format_duration(&repo.last_fetch_at).dimmed().to_string()
                    } else if repo.behind_count > 0 {
                        format_duration(&repo.last_commit_at).dimmed().to_string()
                    } else {
                        format!("{} 个文件", repo.dirty_files.len())
                            .dimmed()
                            .to_string()
                    };

                    table.add_row(vec![
                        repo.name.clone(),
                        repo.branch
                            .clone()
                            .unwrap_or_else(|| "-".to_string())
                            .dimmed()
                            .to_string(),
                        status,
                        detail,
                    ]);
                }

                output.push_str(&table.to_string());
                output.push('\n');
            }
        } else {
            // Small project: show full table
            if !repos.is_empty() {
                let mut table = Table::new();
                table
                    .set_content_arrangement(ContentArrangement::Dynamic)
                    .apply_modifier(UTF8_ROUND_CORNERS)
                    .set_header(vec![
                        "#".cell(),
                        "仓库".cell(),
                        "分支".cell(),
                        "状态".cell(),
                        "远程提交".cell(),
                        "最后更新".cell(),
                    ]);

                for (idx, repo) in repos.iter().enumerate() {
                    let status = format!(
                        "{} {}",
                        repo.freshness.emoji(),
                        if repo.dirty { "+本地修改" } else { "" }
                    );

                    let commits = if repo.behind_count > 0 {
                        format!("落后 {} 个", repo.behind_count).red().to_string()
                    } else if repo.ahead_count > 0 {
                        format!("领先 {} 个", repo.ahead_count).yellow().to_string()
                    } else {
                        "已同步".green().to_string()
                    };

                    let last_update = format_duration(&repo.last_commit_at);

                    table.add_row(vec![
                        (idx + 1).to_string().dimmed().to_string(),
                        repo.name.clone(),
                        repo.branch
                            .clone()
                            .unwrap_or_else(|| "-".to_string())
                            .dimmed()
                            .to_string(),
                        status,
                        commits,
                        last_update.dimmed().to_string(),
                    ]);
                }

                output.push_str(&table.to_string());
                output.push('\n');
            }
        }

        Ok(output)
    }

    fn extension(&self) -> &'static str {
        "txt"
    }
}

/// Print a concise scan summary
pub fn print_scan_summary(repos: &[Repository], summary: &RepoSummary, duration_ms: u128) {
    println!("\n{}", "─".repeat(60).cyan());
    println!(
        "  {} 发现 {} 个仓库（{}ms）",
        "✓".green().bold(),
        repos.len().to_string().cyan().bold(),
        duration_ms
    );

    if summary.has_updates > 0 {
        println!(
            "  {} 个仓库需要更新",
            summary.has_updates.to_string().red().bold()
        );
    }
    if summary.dirty > 0 {
        println!("  {} 个仓库有本地修改", summary.dirty.to_string().yellow());
    }
    if summary.unreachable > 0 {
        println!(
            "  {} 个仓库远程不可达",
            summary.unreachable.to_string().dimmed()
        );
    }

    println!("{}", "─".repeat(60).cyan());
}

/// Print a single repository's details
pub fn print_repo_detail(repo: &Repository) {
    println!("\n{}", "═".repeat(60).cyan());
    println!("  {} {}", "📁".cyan(), repo.name.bold());
    println!("{}", "═".repeat(60).cyan());

    println!("  路径: {}", repo.path.dimmed());
    println!("  分支: {}", repo.branch.as_deref().unwrap_or("-").cyan());

    // Status
    let status_text = match repo.freshness {
        Freshness::HasUpdates => format!(
            "{} 需要更新（落后 {} 个提交）",
            "🔴".red(),
            repo.behind_count
        ),
        Freshness::Synced => format!("{} 已同步", "🟢".green()),
        Freshness::Unreachable => format!("{} 远程不可达", "⚫".dimmed()),
        Freshness::NoRemote => format!("{} 无远程分支", "⚪".white()),
    };
    println!("  状态: {}", status_text);

    if repo.dirty {
        println!(
            "  本地: {} {} 个文件未提交",
            "📝".yellow(),
            repo.dirty_files.len()
        );
    }

    if let Some(ref url) = repo.upstream_url {
        let safe_url = crate::utils::sanitize_url(url);
        println!("  远程: {}", safe_url.dimmed());
    }

    if let Some(ref msg) = repo.last_commit_message {
        println!("\n  最近提交:");
        println!(
            "    {} {}",
            "├─".dimmed(),
            msg.split('\n').next().unwrap_or(msg)
        );
        if let Some(ref author) = repo.last_commit_author {
            println!(
                "    {} {} - {}",
                "└─".dimmed(),
                author.dimmed(),
                format_duration(&repo.last_commit_at).dimmed()
            );
        }
    }

    println!("{}", "═".repeat(60).cyan());
}

/// Print a centralized view of all repositories with issues
pub fn print_issues_view(repos: &[Repository]) {
    use std::path::Path;

    let mut needauth = Vec::new();
    let mut unreachable = Vec::new();
    let mut dirty_behind = Vec::new();
    let mut missing = Vec::new();

    for repo in repos {
        if repo.path.contains(crate::utils::NEEDAUTH_DIR) {
            needauth.push(repo);
            continue;
        }
        // NOTE: Blocking filesystem I/O. For a large number of repos on slow storage,
        // this loop could take a noticeable amount of time. Consider spawn_blocking if needed.
        if !Path::new(&repo.path).exists() {
            missing.push(repo);
            continue;
        }
        if repo.freshness == Freshness::Unreachable {
            unreachable.push(repo);
            continue;
        }
        if repo.dirty || repo.behind_count > 0 {
            dirty_behind.push(repo);
        }
    }

    let total_issues = needauth.len() + unreachable.len() + dirty_behind.len() + missing.len();

    println!("\n{}", "═".repeat(62).cyan());
    println!("  {} {}", "⚠️".yellow(), "仓库问题总览".bold());
    println!("{}", "═".repeat(62).cyan());
    println!(
        "  发现 {} 个问题\n",
        total_issues.to_string().yellow().bold()
    );

    if total_issues == 0 {
        println!("  {} 所有仓库状态良好\n", "✓".green());
        return;
    }

    let print_group = |icon: &str,
                       title: &str,
                       items: &[&Repository],
                       detail_fn: &dyn Fn(&Repository) -> String| {
        if items.is_empty() {
            return;
        }
        println!("  {icon} {} ({})", title.bold(), items.len());
        for (i, repo) in items.iter().enumerate() {
            let is_last = i == items.len() - 1;
            let corner = if is_last { "└─" } else { "├─" };
            let detail = detail_fn(repo);
            println!("     {corner} {} {}", repo.name.cyan(), detail.dimmed());
        }
        println!();
    };

    print_group("🔒", "认证隔离", &needauth, &|repo| {
        format!(
            "[{}]",
            repo.upstream_url
                .as_deref()
                .map(crate::utils::sanitize_url)
                .unwrap_or_else(|| "-".to_string())
        )
    });

    print_group("⚫", "远程不可达", &unreachable, &|repo| {
        format!(
            "[最近 fetch: {}]",
            crate::git::format_duration(&repo.last_fetch_at)
        )
    });

    print_group("📝", "需要关注", &dirty_behind, &|repo| {
        if repo.behind_count > 0 && repo.dirty {
            format!(
                "[落后 {} 个提交, {} 个文件变更]",
                repo.behind_count,
                repo.dirty_files.len()
            )
        } else if repo.behind_count > 0 {
            format!("[落后 {} 个提交]", repo.behind_count)
        } else {
            format!("[{} 个文件变更]", repo.dirty_files.len())
        }
    });

    print_group("❌", "路径缺失", &missing, &|_repo| {
        "[磁盘路径不存在]".to_string()
    });
}

// Helper trait for comfy-table
trait CellExt {
    fn cell(self) -> comfy_table::Cell;
}

impl CellExt for &str {
    fn cell(self) -> comfy_table::Cell {
        comfy_table::Cell::new(self)
    }
}

impl CellExt for String {
    fn cell(self) -> comfy_table::Cell {
        comfy_table::Cell::new(self)
    }
}
