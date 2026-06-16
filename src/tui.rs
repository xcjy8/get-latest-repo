//! 稳定的交互式仓库控制台。
//!
//! 这里刻意不再使用 raw mode / alternate screen：不同终端、主题、复用器和
//! 鼠标模式会产生不同的 ESC 序列，上一版因此出现“按方向键或滚轮就退出”。
//! 当前实现使用普通行输入，所有操作都通过数字菜单触发；方向键、鼠标滚轮
//! 等不可识别输入只会被忽略，不会触发退出或同步动作。

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::Path;

use anyhow::Result;
use colored::Colorize;
use comfy_table::{ContentArrangement, Table, modifiers::UTF8_ROUND_CORNERS};

use crate::git::format_duration;
use crate::models::{Freshness, Repository};

const PAGE_SIZE: usize = 20;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IssueCounts {
    pub local_repairable: usize,
    pub auth_isolated: usize,
    pub remote_issue: usize,
}

impl IssueCounts {
    pub fn total(self) -> usize {
        self.local_repairable + self.auth_isolated + self.remote_issue
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiTab {
    Issues,
    Normal,
}

impl TuiTab {
    fn title(self) -> &'static str {
        match self {
            TuiTab::Issues => "异常仓库",
            TuiTab::Normal => "正常仓库",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiAction {
    Switch(TuiTab),
    RunPullBackup,
    RunFullPullBackup,
    Refresh,
    PrevPage,
    NextPage,
    Quit,
    Ignore,
}

#[derive(Debug, Clone)]
pub struct TuiState {
    pub tab: TuiTab,
    pub page: usize,
    page_size: usize,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            tab: TuiTab::Issues,
            page: 0,
            page_size: PAGE_SIZE,
        }
    }
}

impl TuiState {
    pub fn switch_tab(&mut self, tab: TuiTab) {
        self.tab = tab;
        self.page = 0;
    }

    pub fn prev_page(&mut self) {
        self.page = self.page.saturating_sub(1);
    }

    pub fn next_page(&mut self, total_items: usize) {
        let max_page = total_items.saturating_sub(1) / self.page_size;
        self.page = self.page.saturating_add(1).min(max_page);
    }

    fn page_bounds(&self, total_items: usize) -> (usize, usize) {
        let start = self.page.saturating_mul(self.page_size).min(total_items);
        let end = start.saturating_add(self.page_size).min(total_items);
        (start, end)
    }
}

pub fn render(repos: &[Repository], state: &mut TuiState) -> Result<()> {
    let groups = RepoGroups::from_repos(repos);
    let current = match state.tab {
        TuiTab::Issues => &groups.issues,
        TuiTab::Normal => &groups.normal,
    };
    let max_page = current.len().saturating_sub(1) / state.page_size;
    state.page = state.page.min(max_page);

    print!("\x1b[2J\x1b[H");
    println!("{}", "GetLatestRepo TUI 控制台".cyan().bold());
    println!(
        "{} 总数: {} | 正常: {} | 本地可修复: {} | 认证隔离: {} | 远程异常: {} | 当前: {} | 第 {}/{} 页",
        "📊".cyan(),
        repos.len().to_string().cyan(),
        groups.normal.len().to_string().green(),
        groups.issue_counts.local_repairable.to_string().yellow(),
        groups.issue_counts.auth_isolated,
        groups.issue_counts.remote_issue.to_string().dimmed(),
        state.tab.title().bold(),
        (state.page + 1).to_string().cyan(),
        (max_page + 1).to_string().cyan()
    );
    println!(
        "{}",
        action_hint(
            groups.issue_counts,
            groups.normal.len(),
            groups.issues.len(),
            repos.len()
        )
    );
    println!();
    print_menu();
    println!();

    print_repo_table(current, state)?;
    io::stdout().flush()?;
    Ok(())
}

pub fn read_action() -> Result<TuiAction> {
    print!("\n请选择操作编号: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(parse_action(&input))
}

pub fn confirm_pull_backup(counts: IssueCounts) -> Result<bool> {
    println!();
    println!(
        "{}",
        format!(
            "准备处理 {} 个异常仓库（本地可修复 {} 个，认证隔离 {} 个，远程异常 {} 个）。",
            counts.total(),
            counts.local_repairable,
            counts.auth_isolated,
            counts.remote_issue
        )
        .yellow()
        .bold()
    );
    println!("接下来会发生这些事：");
    println!("1. 只对当前异常列表中的仓库联网获取远程最新状态，等同于执行 fetch。");
    println!("2. 本地可修复仓库会先备份本地改动，再恢复成远程当前状态。");
    println!("3. 认证隔离仓库只会尝试 fetch 和恢复；如果认证仍失败，会继续留在认证隔离列表。");
    println!("4. 远程不可达或无远程分支的仓库会保留诊断状态，不会假装已经同步。");
    println!(
        "结果：未提交修改不会继续留在工作区；如果曾经创建 stash，工具会在输出里提示备份名称。"
    );
    print!("输入 y 开始同步；直接回车或输入其它内容取消: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}

pub fn confirm_full_pull_backup(total_repos: usize, counts: IssueCounts) -> Result<bool> {
    println!();
    println!(
        "{}",
        format!(
            "准备全量同步：当前 TUI 记录 {} 个仓库，其中本地可修复 {} 个，认证隔离 {} 个，远程异常 {} 个。",
            total_repos, counts.local_repairable, counts.auth_isolated, counts.remote_issue
        )
        .yellow()
        .bold()
    );
    println!("接下来会发生这些事：");
    println!("1. 对数据库中启用扫描源下的仓库执行完整同步流程。");
    println!(
        "2. 工作流会先联网 fetch，再重新扫描，然后只对落后远程或有未提交修改的仓库执行备份同步。"
    );
    println!("3. 有未提交修改的仓库会先创建 git stash 备份，再恢复成远程当前状态。");
    println!("4. 认证隔离或远程仍不可达的仓库不会被强行标成正常，会继续保留诊断状态。");
    print!("输入 y 开始全量同步；直接回车或输入其它内容取消: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}

fn print_menu() {
    println!("{}", "[1] 异常  [2] 正常  [3] 修复异常".bold());
    println!("{}", "[4] 刷新  [5] 全量同步  [7] 上页".bold());
    println!("{}", "[8] 下页  [0] 退出".bold());
}

fn action_hint(
    counts: IssueCounts,
    normal_count: usize,
    issue_count: usize,
    total_count: usize,
) -> String {
    if total_count == 0 {
        return format!("{} 暂无仓库记录。", "ℹ".blue());
    }

    if issue_count == 0 {
        return format!(
            "{} 已完成全量同步，当前 {} 个仓库都正常；需要重新同步全部仓库时按 5。",
            "ℹ".blue(),
            normal_count
        );
    }

    if counts.local_repairable > 0 {
        return format!(
            "{} 已完成全量同步，仍有 {} 个本地问题；建议按 3 继续修复。",
            "ℹ".blue(),
            counts.local_repairable
        );
    }

    if counts.auth_isolated > 0 && counts.remote_issue == 0 {
        return format!(
            "{} 已完成全量同步，只剩 {} 个需要登录/授权的仓库；登录后按 3 重试。",
            "ℹ".blue(),
            counts.auth_isolated
        );
    }

    format!(
        "{} 已完成全量同步，当前有 {} 个异常仓库；按 3 处理异常，按 5 重新全量同步。",
        "ℹ".blue(),
        issue_count
    )
}

pub fn wait_for_enter(message: &str) -> Result<()> {
    println!();
    print!("{}，按回车返回 TUI 菜单...", message);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(())
}

pub fn parse_action(input: &str) -> TuiAction {
    match input.trim() {
        "0" | "q" | "Q" => TuiAction::Quit,
        "1" => TuiAction::Switch(TuiTab::Issues),
        "2" => TuiAction::Switch(TuiTab::Normal),
        "3" => TuiAction::RunPullBackup,
        "4" => TuiAction::Refresh,
        "5" => TuiAction::RunFullPullBackup,
        "7" => TuiAction::PrevPage,
        "8" => TuiAction::NextPage,
        _ => TuiAction::Ignore,
    }
}

pub fn current_tab_len(repos: &[Repository], tab: TuiTab) -> usize {
    let groups = RepoGroups::from_repos(repos);
    match tab {
        TuiTab::Issues => groups.issues.len(),
        TuiTab::Normal => groups.normal.len(),
    }
}

pub fn pull_backup_targets(repos: &[Repository]) -> Vec<Repository> {
    let mut seen_paths = HashSet::new();

    repos
        .iter()
        .filter(|repo| is_issue(repo))
        .filter(|repo| seen_paths.insert(repo.path.clone()))
        .cloned()
        .collect()
}

pub fn issue_counts(repos: &[Repository]) -> IssueCounts {
    let mut counts = IssueCounts::default();

    for repo in repos {
        if is_auth_isolated(repo) {
            counts.auth_isolated += 1;
        } else if is_local_repairable(repo) {
            counts.local_repairable += 1;
        } else if is_remote_issue(repo) {
            counts.remote_issue += 1;
        }
    }

    counts
}

struct RepoGroups<'a> {
    issues: Vec<&'a Repository>,
    normal: Vec<&'a Repository>,
    issue_counts: IssueCounts,
}

impl<'a> RepoGroups<'a> {
    fn from_repos(repos: &'a [Repository]) -> Self {
        let mut issues = Vec::new();
        let mut normal = Vec::new();
        let mut issue_counts = IssueCounts::default();

        for repo in repos {
            if is_auth_isolated(repo) {
                issue_counts.auth_isolated += 1;
                issues.push(repo);
            } else if is_local_repairable(repo) {
                issue_counts.local_repairable += 1;
                issues.push(repo);
            } else if is_remote_issue(repo) {
                issue_counts.remote_issue += 1;
                issues.push(repo);
            } else {
                normal.push(repo);
            }
        }

        Self {
            issues,
            normal,
            issue_counts,
        }
    }
}

fn print_repo_table(repos: &[&Repository], state: &TuiState) -> Result<()> {
    if repos.is_empty() {
        println!("{}", "当前分类没有仓库。".dimmed());
        return Ok(());
    }

    let (start, end) = state.page_bounds(repos.len());
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec!["序号", "仓库", "分支", "状态", "详情"]);

    for (idx, repo) in repos[start..end].iter().enumerate() {
        table.add_row(vec![
            (start + idx + 1).to_string(),
            repo.name.clone(),
            repo.branch.as_deref().unwrap_or("-").to_string(),
            status_text(repo),
            status_detail(repo),
        ]);
    }

    println!("{table}");
    Ok(())
}

fn is_issue(repo: &Repository) -> bool {
    is_auth_isolated(repo) || is_local_repairable(repo) || is_remote_issue(repo)
}

fn is_local_repairable(repo: &Repository) -> bool {
    !is_auth_isolated(repo)
        && (repo_path_missing(repo)
            || repo.dirty
            || repo.behind_count > 0
            || repo.freshness == Freshness::HasUpdates)
}

fn is_auth_isolated(repo: &Repository) -> bool {
    Path::new(&repo.path)
        .components()
        .any(|component| component.as_os_str().to_string_lossy() == crate::utils::NEEDAUTH_DIR)
}

fn is_remote_issue(repo: &Repository) -> bool {
    matches!(repo.freshness, Freshness::Unreachable | Freshness::NoRemote)
}

fn status_text(repo: &Repository) -> String {
    if repo_path_missing(repo) {
        return "路径不存在".to_string();
    }
    if is_auth_isolated(repo) {
        if repo.dirty {
            return format!("需要登录/授权 + 本地修改 {} 个文件", repo.dirty_files.len());
        }
        return "需要登录/授权".to_string();
    }
    if repo.behind_count > 0 && repo.dirty {
        return "落后 + 本地修改".to_string();
    }
    if repo.behind_count > 0 || repo.freshness == Freshness::HasUpdates {
        return format!("落后 {} 个提交", repo.behind_count);
    }
    if repo.dirty {
        return format!("本地修改 {} 个文件", repo.dirty_files.len());
    }
    match repo.freshness {
        Freshness::Synced => "已同步".to_string(),
        Freshness::Unreachable => "远程不可达".to_string(),
        Freshness::NoRemote => "无远程分支".to_string(),
        Freshness::HasUpdates => "需要更新".to_string(),
    }
}

fn status_detail(repo: &Repository) -> String {
    if repo_path_missing(repo) {
        return crate::utils::sanitize_path(&repo.path);
    }
    if is_auth_isolated(repo) && repo.dirty {
        return format!("需授权；本地改动 {} 个文件", repo.dirty_files.len());
    }
    if is_auth_isolated(repo) && !repo.dirty {
        if let Some(pull_at) = &repo.last_pull_at {
            return format!(
                "仓库仍在本机，最近处理 {}",
                format_duration(&Some(*pull_at))
            );
        }
        return "仓库仍在本机，等待授权".to_string();
    }
    if repo.dirty {
        return format!("{} 个文件", repo.dirty_files.len());
    }
    if repo.behind_count > 0 {
        return format!("最近提交 {}", format_duration(&repo.last_commit_at));
    }
    if let Some(fetch_at) = &repo.last_fetch_at {
        return format!("最近 fetch {}", format_duration(&Some(*fetch_at)));
    }
    "无 fetch 记录".to_string()
}

fn repo_path_missing(repo: &Repository) -> bool {
    !Path::new(&repo.path).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str, freshness: Freshness, dirty: bool, behind_count: i32) -> Repository {
        repo_at(
            name,
            &std::env::current_dir().unwrap().to_string_lossy(),
            freshness,
            dirty,
            behind_count,
        )
    }

    fn repo_at(
        name: &str,
        path: &str,
        freshness: Freshness,
        dirty: bool,
        behind_count: i32,
    ) -> Repository {
        Repository {
            name: name.to_string(),
            path: path.to_string(),
            freshness,
            dirty,
            behind_count,
            dirty_files: if dirty {
                vec!["changed.txt".to_string()]
            } else {
                Vec::new()
            },
            ..Repository::default()
        }
    }

    fn needauth_repo_at(name: &str, path: &str, dirty: bool) -> Repository {
        Repository {
            name: name.to_string(),
            path: path.to_string(),
            freshness: Freshness::Synced,
            dirty,
            dirty_files: if dirty {
                vec!["changed.txt".to_string()]
            } else {
                Vec::new()
            },
            ..Repository::default()
        }
    }

    #[test]
    fn parse_numeric_actions() {
        assert_eq!(parse_action("1\n"), TuiAction::Switch(TuiTab::Issues));
        assert_eq!(parse_action("2\n"), TuiAction::Switch(TuiTab::Normal));
        assert_eq!(parse_action("3\n"), TuiAction::RunPullBackup);
        assert_eq!(parse_action("4\n"), TuiAction::Refresh);
        assert_eq!(parse_action("5\n"), TuiAction::RunFullPullBackup);
        assert_eq!(parse_action("7\n"), TuiAction::PrevPage);
        assert_eq!(parse_action("8\n"), TuiAction::NextPage);
        assert_eq!(parse_action("0\n"), TuiAction::Quit);
    }

    #[test]
    fn unknown_escape_sequences_are_ignored() {
        assert_eq!(parse_action("\u{1b}[A\n"), TuiAction::Ignore);
        assert_eq!(parse_action("\u{1b}[<64;10;10M\n"), TuiAction::Ignore);
    }

    #[test]
    fn categorizes_normal_and_issue_repos() {
        let repos = vec![
            repo("clean", Freshness::Synced, false, 0),
            repo("dirty", Freshness::Synced, true, 0),
            repo("behind", Freshness::HasUpdates, false, 2),
        ];

        assert_eq!(current_tab_len(&repos, TuiTab::Normal), 1);
        assert_eq!(current_tab_len(&repos, TuiTab::Issues), 2);
    }

    #[test]
    fn counts_local_repairable_and_auth_isolated_separately() {
        let temp = tempfile::tempdir().unwrap();
        let clean_path = temp.path().join("clean");
        let dirty_path = temp.path().join("dirty");
        let auth_path = temp.path().join(crate::utils::NEEDAUTH_DIR).join("auth");
        std::fs::create_dir_all(&clean_path).unwrap();
        std::fs::create_dir_all(&dirty_path).unwrap();
        std::fs::create_dir_all(&auth_path).unwrap();

        let repos = vec![
            repo_at(
                "clean",
                &clean_path.to_string_lossy(),
                Freshness::Synced,
                false,
                0,
            ),
            repo_at(
                "dirty",
                &dirty_path.to_string_lossy(),
                Freshness::Synced,
                true,
                0,
            ),
            needauth_repo_at("auth", &auth_path.to_string_lossy(), false),
        ];

        let counts = issue_counts(&repos);

        assert_eq!(counts.local_repairable, 1);
        assert_eq!(counts.auth_isolated, 1);
        assert_eq!(counts.remote_issue, 0);
        assert_eq!(counts.total(), 2);
    }

    #[test]
    fn clean_needauth_status_explains_auth_isolation() {
        let temp = tempfile::tempdir().unwrap();
        let auth_path = temp
            .path()
            .join(crate::utils::NEEDAUTH_DIR)
            .join("auth-clean");
        std::fs::create_dir_all(&auth_path).unwrap();
        let repo = needauth_repo_at("auth-clean", &auth_path.to_string_lossy(), false);

        assert!(status_text(&repo).contains("需要登录/授权"));
        assert!(status_detail(&repo).contains("仓库仍在本机"));
        assert!(status_detail(&repo).contains("等待授权"));
    }

    #[test]
    fn pull_backup_targets_only_include_issue_repos() {
        let temp = tempfile::tempdir().unwrap();
        let clean_path = temp.path().join("clean");
        let dirty_path = temp.path().join("dirty");
        let behind_path = temp.path().join("behind");
        std::fs::create_dir_all(&clean_path).unwrap();
        std::fs::create_dir_all(&dirty_path).unwrap();
        std::fs::create_dir_all(&behind_path).unwrap();

        let repos = vec![
            repo_at(
                "clean",
                &clean_path.to_string_lossy(),
                Freshness::Synced,
                false,
                0,
            ),
            repo_at(
                "dirty",
                &dirty_path.to_string_lossy(),
                Freshness::Synced,
                true,
                0,
            ),
            repo_at(
                "behind",
                &behind_path.to_string_lossy(),
                Freshness::HasUpdates,
                false,
                1,
            ),
        ];

        let targets = pull_backup_targets(&repos);

        assert_eq!(targets.len(), 2);
        assert!(targets.iter().any(|repo| repo.name == "dirty"));
        assert!(targets.iter().any(|repo| repo.name == "behind"));
        assert!(!targets.iter().any(|repo| repo.name == "clean"));
    }

    #[test]
    fn pagination_is_bounded() {
        let mut state = TuiState::default();
        state.next_page(10);
        assert_eq!(state.page, 0);

        state.next_page(50);
        assert_eq!(state.page, 1);
        state.prev_page();
        assert_eq!(state.page, 0);
    }
}
