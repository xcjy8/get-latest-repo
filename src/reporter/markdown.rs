use anyhow::Result;

use super::Reporter;
use crate::git::format_duration;
use crate::models::{Freshness, RepoSummary, Repository};

pub struct MarkdownReporter;

impl MarkdownReporter {
    pub fn new() -> Self {
        Self
    }
}

impl Reporter for MarkdownReporter {
    fn generate(&self, repos: &[Repository], summary: &RepoSummary) -> Result<String> {
        let mut md = String::new();
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");

        // Title
        md.push_str("# 📦 GetLatestRepo 扫描报告\n\n");
        md.push_str(&format!("生成时间：{}\n\n", now));

        // Summary
        md.push_str("## 📊 摘要\n\n");
        md.push_str(&format!("- **仓库总数**：{}\n", summary.total));
        md.push_str(&format!("- 🔴 需要更新：{}\n", summary.has_updates));
        md.push_str(&format!("- 🟢 已同步：{}\n", summary.synced));
        md.push_str(&format!("- 🟡 本地修改：{}\n", summary.dirty));
        md.push_str(&format!("- ⚫ 远程不可达：{}\n", summary.unreachable));
        md.push_str(&format!("- ⚪ 无远程分支：{}\n", summary.no_remote));
        md.push('\n');

        // Repository list
        if !repos.is_empty() {
            md.push_str("## 📋 仓库详情\n\n");
            md.push_str("| 仓库 | 分支 | 状态 | 落后 | 本地修改 | 最后更新 |\n");
            md.push_str("|------|------|------|--------|----------|----------|\n");

            for repo in repos {
                let status = match repo.freshness {
                    Freshness::HasUpdates => "🔴 需要更新",
                    Freshness::Synced => "🟢 已同步",
                    Freshness::Unreachable => "⚫ 远程不可达",
                    Freshness::NoRemote => "⚪ 无远程分支",
                };

                let dirty_mark = if repo.dirty { "📝 是" } else { "-" };
                let behind = if repo.behind_count > 0 {
                    format!("{} 个提交", repo.behind_count)
                } else {
                    "-".to_string()
                };

                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} |\n",
                    repo.name,
                    repo.branch.as_deref().unwrap_or("-"),
                    status,
                    behind,
                    dirty_mark,
                    format_duration(&repo.last_commit_at)
                ));
            }

            md.push('\n');
        }

        // Repositories needing attention
        let needs_attention: Vec<_> = repos
            .iter()
            .filter(|r| r.freshness == Freshness::HasUpdates || r.dirty)
            .collect();

        if !needs_attention.is_empty() {
            md.push_str("## ⚠️ 需要关注的仓库\n\n");

            for repo in needs_attention {
                md.push_str(&format!("### {}\n\n", repo.name));
                md.push_str(&format!(
                    "- **路径**：`{}`\n",
                    crate::utils::sanitize_path(&repo.path)
                ));
                md.push_str(&format!(
                    "- **分支**：`{}`\n",
                    repo.branch.as_deref().unwrap_or("无")
                ));

                if repo.freshness == Freshness::HasUpdates {
                    md.push_str(&format!("- **落后**：{} 个提交\n", repo.behind_count));
                }

                if repo.dirty {
                    md.push_str(&format!(
                        "- **本地修改**：{} 个文件\n",
                        repo.dirty_files.len()
                    ));
                    md.push_str("- **已修改文件**：\n");
                    for file in &repo.dirty_files {
                        md.push_str(&format!("  - `{}`\n", file));
                    }
                }

                if let Some(ref msg) = repo.last_commit_message {
                    md.push_str(&format!("- **最近提交**：{}\n", msg.trim()));
                }

                md.push('\n');
            }
        }

        // Footer
        md.push_str("---\n\n");
        md.push_str("*由 [GetLatestRepo](https://github.com/xcjy8/GetLatestRepo) 生成*\n");

        Ok(md)
    }

    fn extension(&self) -> &'static str {
        "md"
    }
}
