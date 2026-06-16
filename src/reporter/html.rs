use anyhow::Result;
use askama::Template;

use super::Reporter;
use crate::git::format_duration;
use crate::models::{Freshness, RepoSummary, Repository};

#[derive(Template)]
#[template(path = "report.html")]
struct HtmlTemplate {
    repos: Vec<RepositoryView>,
    summary: RepoSummary,
    generated_at: String,
}

/// HTML report repository view
///
/// Note: some fields are for HTML template use, Rust compiler cannot detect usage in template
#[derive(Debug)]
#[allow(dead_code)]
struct RepositoryView {
    name: String,
    path: String,
    branch: String,
    status_class: String,
    status_icon: String,
    status_text: String,
    behind_count: i32,
    /// Currently not used in template, reserved for future extension
    ahead_count: i32,
    is_dirty: bool,
    dirty_count: usize,
    dirty_files: Vec<String>,
    last_commit_message: String,
    last_commit_author: String,
    last_update: String,
    /// Currently not used in template, reserved for future extension
    upstream_url: Option<String>,
}

impl From<&Repository> for RepositoryView {
    fn from(repo: &Repository) -> Self {
        let (status_class, status_icon, status_text) = match repo.freshness {
            Freshness::HasUpdates => ("status-behind", "🔴", "需要更新"),
            Freshness::Synced => ("status-synced", "🟢", "已同步"),
            Freshness::Unreachable => ("status-error", "⚫", "远程不可达"),
            Freshness::NoRemote => ("status-none", "⚪", "无远程分支"),
        };

        Self {
            name: repo.name.clone(),
            path: crate::utils::sanitize_path(&repo.path),
            branch: repo.branch.clone().unwrap_or_else(|| "-".to_string()),
            status_class: status_class.to_string(),
            status_icon: status_icon.to_string(),
            status_text: status_text.to_string(),
            behind_count: repo.behind_count,
            ahead_count: repo.ahead_count,
            is_dirty: repo.dirty,
            dirty_count: repo.dirty_files.len(),
            dirty_files: repo.dirty_files.clone(),
            last_commit_message: repo
                .last_commit_message
                .clone()
                .unwrap_or_else(|| "-".to_string()),
            last_commit_author: repo
                .last_commit_author
                .clone()
                .unwrap_or_else(|| "-".to_string()),
            last_update: format_duration(&repo.last_commit_at),
            upstream_url: repo
                .upstream_url
                .as_ref()
                .map(|url| crate::utils::sanitize_url(url)),
        }
    }
}

pub struct HtmlReporter;

impl HtmlReporter {
    pub fn new() -> Self {
        Self
    }
}

impl Reporter for HtmlReporter {
    fn generate(&self, repos: &[Repository], summary: &RepoSummary) -> Result<String> {
        let template = HtmlTemplate {
            repos: repos.iter().map(RepositoryView::from).collect(),
            summary: summary.clone(),
            generated_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        };

        Ok(template.render()?)
    }

    fn extension(&self) -> &'static str {
        "html"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_report_uses_status_class_without_duplicate_prefix() {
        let repo = Repository {
            name: "demo".to_string(),
            freshness: Freshness::HasUpdates,
            behind_count: 1,
            ..Repository::default()
        };
        let mut summary = RepoSummary::new();
        summary.add(&repo);

        let html = HtmlReporter::new()
            .generate(&[repo], &summary)
            .expect("HTML 报告应成功渲染");

        assert!(html.contains("class=\"badge status-behind\""));
        assert!(!html.contains("status-status-behind"));
    }
}
