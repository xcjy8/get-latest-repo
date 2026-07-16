use anyhow::Result;
use colored::*;
use futures::StreamExt;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crate::cli::OutputFormat;
use crate::config::AppConfig;
use crate::db::Database;
use crate::fetcher::{FetchSummary, Fetcher};
use crate::git::ProxyConfig;
use crate::models::{Freshness, RepoSummary};
use crate::scanner::Scanner;
use crate::security::{SecurityScanner, format_security_report};

use super::types::*;

type RepositoryProgressObserver = Arc<dyn Fn(usize, usize) + Send + Sync>;
type PhaseProgressObserver = Arc<dyn Fn(&'static str, usize, usize) + Send + Sync>;

/// 交互式安全确认的批量选择结果。
///
/// Pull 前安全扫描可能一次命中几十甚至上百个高风险仓库。逐个询问会让操作者
/// 难以快速决策，因此这里统一使用“先汇总、再输入序号”的模型：
/// - `All`：输入 `0`，表示所有高风险仓库都继续执行。
/// - `Some`：输入 `1,3,5` 或 `1 3 5`，只继续对应序号，其余跳过。
/// - `None`：直接回车，表示全部跳过。
#[derive(Debug, Clone, PartialEq, Eq)]
enum BatchRiskSelection {
    All,
    Some(std::collections::HashSet<usize>),
    None,
}

/// 工作流的仓库限定范围。
///
/// 普通命令行工作流不设置该范围，继续按扫描源处理全部仓库。TUI 菜单 3
/// 会把当前异常列表传进来，执行器据此限制 fetch、scan 和 pull 的输入。
/// 这里同时记录 `id`、`path` 和 “扫描根 + 仓库名 + upstream_url”：
/// - `id` 能覆盖同一次运行内未移动的数据库记录；
/// - `path` 能覆盖路径稳定的仓库，也能保留无远程仓库、本地脏仓库等没有
///   upstream_url 的记录；
/// - `upstream_url` 兜底处理 fetch 后从 `needauth/` 恢复或移动导致数据库
///   路径变化的仓库，避免限定范围在工作流中途丢失。
#[derive(Debug, Clone, Default)]
struct RepositoryScope {
    ids: HashSet<i64>,
    paths: HashSet<String>,
    upstream_keys: HashSet<RepositoryUpstreamKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RepositoryUpstreamKey {
    scope_root_path: String,
    name: String,
    upstream_url: String,
}

impl RepositoryScope {
    fn from_repositories(repos: impl IntoIterator<Item = crate::models::Repository>) -> Self {
        let mut scope = Self::default();

        for repo in repos {
            if let Some(id) = repo.id {
                scope.ids.insert(id);
            }

            if !repo.path.is_empty() {
                scope.paths.insert(repo.path.clone());
            }

            if let Some(upstream_url) = repo.upstream_url
                && !repo.name.is_empty()
                && !upstream_url.is_empty()
            {
                scope.upstream_keys.insert(RepositoryUpstreamKey {
                    scope_root_path: Self::normalized_scope_root(&repo.root_path),
                    name: repo.name,
                    upstream_url,
                });
            }
        }

        scope
    }

    fn contains(&self, repo: &crate::models::Repository) -> bool {
        if let Some(id) = repo.id
            && self.ids.contains(&id)
        {
            return true;
        }

        if self.paths.contains(&repo.path) {
            return true;
        }

        if let Some(upstream_url) = &repo.upstream_url {
            return self.upstream_keys.contains(&RepositoryUpstreamKey {
                scope_root_path: Self::normalized_scope_root(&repo.root_path),
                name: repo.name.clone(),
                upstream_url: upstream_url.clone(),
            });
        }

        false
    }

    fn normalized_scope_root(root_path: &str) -> String {
        let path = std::path::Path::new(root_path);
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == crate::utils::NEEDAUTH_DIR)
            && let Some(parent) = path.parent()
        {
            return parent.to_string_lossy().to_string();
        }

        root_path.to_string()
    }

    fn len(&self) -> usize {
        self.paths
            .len()
            .max(self.ids.len())
            .max(self.upstream_keys.len())
    }

    fn is_empty(&self) -> bool {
        self.ids.is_empty() && self.paths.is_empty() && self.upstream_keys.is_empty()
    }
}

enum ScopedScanOutcome {
    Fresh(crate::models::Repository),
    Stale {
        repo: crate::models::Repository,
        reason: String,
    },
}

/// 解析批量安全确认输入。
///
/// `total` 是本次展示的高风险仓库数量，序号从 1 开始。非法序号或无法解析的
/// token 会返回错误，让调用方重新提示，避免误把用户输错的内容当成确认。
fn parse_batch_risk_selection(input: &str, total: usize) -> Result<BatchRiskSelection> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(BatchRiskSelection::None);
    }

    let tokens: Vec<_> = trimmed
        .split(|c: char| c == ',' || c == '，' || c.is_whitespace())
        .filter(|token| !token.is_empty())
        .collect();

    if tokens.contains(&"0") {
        return Ok(BatchRiskSelection::All);
    }

    let mut selected = std::collections::HashSet::new();
    for token in tokens {
        let index = token
            .parse::<usize>()
            .map_err(|_| anyhow::anyhow!("无法识别的序号: {}", token))?;
        if index == 0 || index > total {
            anyhow::bail!("序号 {} 超出范围，应在 1..={} 之间", index, total);
        }
        selected.insert(index);
    }

    Ok(BatchRiskSelection::Some(selected))
}

/// Unified view for printing repository change trees (used by both Repository and DirtyRepoInfo)
trait RepoChangeView {
    fn name(&self) -> &str;
    fn path(&self) -> &str;
    fn branch(&self) -> Option<&str>;
    fn file_changes(&self) -> &[crate::models::FileChange];
    fn change_summary(&self) -> String;
}

impl RepoChangeView for crate::models::Repository {
    fn name(&self) -> &str {
        &self.name
    }
    fn path(&self) -> &str {
        &self.path
    }
    fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }
    fn file_changes(&self) -> &[crate::models::FileChange] {
        &self.file_changes
    }
    fn change_summary(&self) -> String {
        self.change_summary()
    }
}

impl RepoChangeView for crate::workflow::types::DirtyRepoInfo {
    fn name(&self) -> &str {
        &self.name
    }
    fn path(&self) -> &str {
        &self.path
    }
    fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }
    fn file_changes(&self) -> &[crate::models::FileChange] {
        &self.file_changes
    }
    fn change_summary(&self) -> String {
        self.change_summary()
    }
}

/// Convert a Repository into a DirtyRepoInfo
fn repo_to_dirty_info(r: crate::models::Repository) -> DirtyRepoInfo {
    DirtyRepoInfo::new(r.name, r.path, r.branch.clone(), r.file_changes.clone())
}

/// Print a single repository's change tree (shared between execute() and execute_pull_safe())
fn print_repo_change_tree(repo: &impl RepoChangeView, is_last: bool, base_indent: usize) {
    let pad = " ".repeat(base_indent);
    let repo_connector = if is_last { "└─" } else { "├─" };
    println!("{}{} 📦 {}", pad, repo_connector, repo.name().bold());

    let meta = if is_last { "      " } else { "   │  " };
    println!("{}📁 {}", meta, repo.path().dimmed());

    let branch_info = repo.branch().unwrap_or("未知");
    println!(
        "{}🌿 分支: {} | 状态: {}",
        meta,
        branch_info.cyan(),
        repo.change_summary().yellow()
    );

    if !repo.file_changes().is_empty() {
        println!("{}📝 变更文件（{}）:", meta, repo.file_changes().len());

        for (j, change) in repo.file_changes().iter().enumerate() {
            let is_last_file = j == repo.file_changes().len() - 1;
            let file_pad = if is_last { "       " } else { "   │   " };
            let file_tree = if is_last_file { "└─" } else { "├─" };

            let status_icon = match change.status.as_str() {
                "added" => "✚",
                "deleted" => "✗",
                "modified" => "✎",
                "renamed" => "➜",
                _ => "?",
            };

            println!(
                "{}{} {} {} {}",
                file_pad,
                file_tree,
                status_icon,
                change.path,
                if change.staged {
                    "（已暂存）".green()
                } else {
                    "（未暂存）".dimmed()
                }
            );

            let detail = if is_last_file {
                "         "
            } else {
                "   │     "
            };
            println!("{}影响: {}", detail, change.impact.dimmed());
            println!(
                "{}执行 pull-force 后: {}",
                detail,
                change.stash_effect.dimmed()
            );

            if !is_last_file {
                println!("{}", file_pad);
            }
        }
    }
}

/// Workflow executor
pub struct WorkflowExecutor {
    workflow: Workflow,
    jobs_override: Option<usize>,
    timeout_override: Option<u64>,
    dry_run: bool,
    silent: bool,
    security_check: bool,
    auto_skip_high_risk: bool,
    auto_allow_high_risk: bool,
    pull_safety_check: bool, // Pull safety check (prevent repo deletion)
    proxy: ProxyConfig,
    target_scope: Option<RepositoryScope>,
    cancellation_checker: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    progress_observer: Option<RepositoryProgressObserver>,
    phase_progress_observer: Option<PhaseProgressObserver>,
    defer_status_refresh: bool,
}

impl WorkflowExecutor {
    pub fn new(
        workflow: Workflow,
        jobs: Option<usize>,
        timeout: Option<u64>,
        dry_run: bool,
        silent: bool,
    ) -> Self {
        Self {
            jobs_override: jobs,
            timeout_override: timeout,
            workflow,
            dry_run,
            silent,
            security_check: true, // Enabled by default
            auto_skip_high_risk: false,
            auto_allow_high_risk: false,
            pull_safety_check: true, // Enabled repo-deletion detection by default
            proxy: ProxyConfig::default(),
            target_scope: None,
            cancellation_checker: None,
            progress_observer: None,
            phase_progress_observer: None,
            defer_status_refresh: false,
        }
    }

    /// 限定本次工作流只处理指定仓库。
    ///
    /// 这个接口由 TUI 使用：菜单 3 的语义是“处理当前异常列表”，不是重新对
    /// 全库执行 `pull-backup`。限定范围放在执行器层，是为了让 fetch、scan、
    /// pull 三个阶段都共享同一条边界，避免只限制最后一步造成前面仍然联网
    /// fetch 341 个仓库。
    pub fn with_target_repositories(
        mut self,
        repos: impl IntoIterator<Item = crate::models::Repository>,
    ) -> Self {
        self.target_scope = Some(RepositoryScope::from_repositories(repos));
        self
    }

    /// Set whether to enable the security scan
    pub fn with_security_check(mut self, enable: bool) -> Self {
        self.security_check = enable;
        self
    }

    /// Set whether to automatically skip high-risk repositories
    pub fn with_auto_skip_high_risk(mut self, enable: bool) -> Self {
        self.auto_skip_high_risk = enable;
        if enable {
            self.auto_allow_high_risk = false;
        }
        self
    }

    /// 自动放行确认命中的高风险仓库；默认关闭，CLI/脚本继续严格确认。
    pub fn with_auto_allow_high_risk(mut self, enable: bool) -> Self {
        self.auto_allow_high_risk = enable;
        if enable {
            self.auto_skip_high_risk = false;
        }
        self
    }

    /// Set whether to enable the pull safety check (repo-deletion detection)
    pub fn with_pull_safety_check(mut self, enable: bool) -> Self {
        self.pull_safety_check = enable;
        self
    }

    /// Set proxy
    pub fn with_proxy(mut self, proxy: ProxyConfig) -> Self {
        self.proxy = proxy;
        self
    }

    /// 注册协作式取消检查器；Web 控制台可终止尚未开始的阶段与仓库任务。
    pub fn with_cancellation_checker(
        mut self,
        checker: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        self.cancellation_checker = Some(checker);
        self
    }

    /// 注册仓库级进度观察器；主要用于实时控制台展示工作流中的 fetch 阶段。
    pub fn with_progress_observer(mut self, observer: RepositoryProgressObserver) -> Self {
        self.progress_observer = Some(observer);
        self
    }

    /// 注册阶段级进度观察器；安全扫描等准备阶段可独立展示真实进度。
    pub fn with_phase_progress_observer(mut self, observer: PhaseProgressObserver) -> Self {
        self.phase_progress_observer = Some(observer);
        self
    }

    /// 延后逐仓库状态刷新；调用方必须在工作流结束后执行一次权威批量复检。
    pub fn with_deferred_status_refresh(mut self, defer: bool) -> Self {
        self.defer_status_refresh = defer;
        self
    }

    /// CLI 中断信号与 Web 取消请求共享一条判断路径，避免两个入口行为分叉。
    fn is_cancelled(&self) -> bool {
        crate::signal_handler::is_shutdown_requested()
            || self
                .cancellation_checker
                .as_ref()
                .is_some_and(|checker| checker())
    }

    fn has_target_scope(&self) -> bool {
        self.target_scope.is_some()
    }

    fn target_scope_len(&self) -> usize {
        self.target_scope
            .as_ref()
            .map(RepositoryScope::len)
            .unwrap_or(0)
    }

    /// CLI 显式覆盖值优先，其次才是步骤值和工作流默认值。
    fn resolve_jobs(&self, step_value: Option<usize>) -> usize {
        self.jobs_override
            .or(step_value)
            .unwrap_or(self.workflow.default_jobs)
    }

    fn resolve_timeout(&self, step_value: Option<u64>) -> u64 {
        self.timeout_override
            .or(step_value)
            .unwrap_or(self.workflow.default_timeout)
    }

    /// 根据当前执行范围筛选数据库仓库记录。
    ///
    /// 未设置限定范围时，工作流沿用历史行为：只处理启用扫描源下的仓库。
    /// 设置限定范围时，来源是 TUI 已展示给用户的异常列表，因此这里优先按
    /// 目标集合过滤，而不是再次扩展为扫描源下的全量仓库。
    fn repositories_for_scope(
        &self,
        db: &Database,
        sources: &[crate::models::ScanSource],
    ) -> Result<Vec<crate::models::Repository>> {
        let all_repos = db.list_repositories()?;

        if let Some(scope) = &self.target_scope {
            if scope.is_empty() {
                return Ok(Vec::new());
            }

            return Ok(all_repos
                .into_iter()
                .filter(|repo| scope.contains(repo))
                .collect());
        }

        let source_paths: HashSet<_> = sources.iter().map(|s| s.root_path.as_str()).collect();
        Ok(all_repos
            .into_iter()
            .filter(|repo| source_paths.contains(repo.root_path.as_str()))
            .collect())
    }

    /// Execute the workflow
    pub async fn execute(&self) -> Result<WorkflowResult> {
        let start = Instant::now();

        if !self.silent {
            let title = format!("▶ 工作流: {}", self.workflow.name);
            let desc = &self.workflow.description;
            println!("\n┌────────────────────────────────────────────────────────────┐");
            println!("│ {:<58} │", title.bold());
            println!("│ {:<58} │", desc.dimmed());
            println!("└────────────────────────────────────────────────────────────┘");
            if self.has_target_scope() {
                println!(
                    "  {} 本次仅处理限定范围内的 {} 个仓库",
                    "ℹ".blue(),
                    self.target_scope_len()
                );
            }
            println!();
        }

        if self.dry_run {
            self.print_dry_run();
            return Ok(WorkflowResult::success());
        }

        // Check initialization
        let config = AppConfig::load()?;
        if !config.is_initialized() {
            anyhow::bail!("尚未初始化，请先运行: getlatestrepo init <path>");
        }

        let db = Database::open()?;
        let sources = config.scan_sources;

        if sources.is_empty() {
            anyhow::bail!("没有启用的扫描源");
        }

        let mut result = WorkflowResult::success();
        let total_steps = self.workflow.steps.len();

        for (idx, step) in self.workflow.steps.iter().enumerate() {
            // Check for graceful shutdown request before starting each step
            if self.is_cancelled() {
                if !self.silent {
                    println!("  {} 用户中断工作流，提前停止...", "⚠️".yellow());
                }
                result.add_error("用户中断工作流".to_string());
                break;
            }

            let step_num = idx + 1;

            match step {
                WorkflowStep::Fetch { jobs, timeout } => {
                    let jobs = self.resolve_jobs(*jobs);
                    let timeout = self.resolve_timeout(*timeout);

                    if !self.silent {
                        let scope_name = if self.has_target_scope() {
                            "Fetch 限定仓库"
                        } else {
                            "Fetch 所有仓库"
                        };
                        println!(
                            "  [{}] {}",
                            format!("{}/{}", step_num, total_steps).cyan(),
                            scope_name
                        );
                    }

                    match self.execute_fetch(&db, &sources, jobs, timeout).await {
                        Ok(summary) => {
                            if !self.silent {
                                // Proxy info
                                if self.proxy.enabled {
                                    println!(
                                        "  ├─ {} {}",
                                        "ℹ".blue(),
                                        self.proxy.http_proxy.dimmed()
                                    );
                                }

                                // Progress bar
                                println!(
                                    "  ├─ ████████████████████████████████████████ {:>2}/{}",
                                    summary.total, summary.total
                                );

                                // Result statistics
                                let success_str = format!("{}", summary.success).green();
                                let failed_str = if summary.failed > 0 {
                                    format!("{}", summary.failed).red()
                                } else {
                                    format!("{}", summary.failed).green()
                                };
                                println!(
                                    "  ├─ {} 总计: {} | 成功: {} | 失败: {}",
                                    "▶".blue(),
                                    summary.total,
                                    success_str,
                                    failed_str
                                );

                                // Failed details (tree view)
                                if summary.failed > 0 {
                                    println!("  │");
                                    println!("  └─ {} 失败详情:", "⚠".yellow());
                                    let failed_repos: Vec<_> =
                                        summary.results.iter().filter(|r| !r.success).collect();
                                    for (i, repo) in failed_repos.iter().enumerate() {
                                        let is_last = i == failed_repos.len() - 1;
                                        let corner = if is_last { "└─" } else { "├─" };

                                        let error_msg = repo.error.as_deref().unwrap_or("未知错误");
                                        let short_error = if error_msg.chars().count() > 42 {
                                            let truncated: String =
                                                error_msg.chars().take(42).collect();
                                            format!("{truncated}...")
                                        } else {
                                            error_msg.to_string()
                                        };
                                        let short_path = std::path::Path::new(&repo.repo_path)
                                            .file_name()
                                            .and_then(|n| n.to_str())
                                            .unwrap_or(&repo.repo_path);
                                        println!(
                                            "     {} {} {}: {}",
                                            corner,
                                            short_path,
                                            "𐄂".dimmed(),
                                            short_error.dimmed()
                                        );
                                    }
                                }
                                println!();
                            }

                            if summary.failed > 0 {
                                result.add_error(format!(
                                    "Fetch 未完整成功：{} 个仓库失败",
                                    summary.failed
                                ));
                                // 后续 Pull/reset 必须基于完整的新鲜远程状态；部分失败时立即停止。
                                if workflow_steps_modify_repositories(
                                    &self.workflow.steps[step_num..],
                                ) {
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            if !self.silent {
                                println!("  └─ {} {}", "✗".red(), e);
                            }
                            result.add_error(format!("Fetch 失败: {}", e));
                        }
                    }
                }

                WorkflowStep::Scan {
                    output,
                    open,
                    only_dirty_or_behind,
                } => {
                    if !self.silent {
                        let output_name = match output {
                            OutputFormat::Terminal => "终端",
                            OutputFormat::Html => "HTML",
                            OutputFormat::Markdown => "Markdown",
                        };
                        print!(
                            "[{}] 扫描并生成 {} 报告... ",
                            format!("{}/{}", step_num, total_steps).cyan(),
                            output_name
                        );
                    }

                    match self
                        .execute_scan(&db, &sources, *output, *open, *only_dirty_or_behind)
                        .await
                    {
                        Ok(summary) => {
                            if !self.silent {
                                println!("{} {} 个仓库", "✓".green(), summary.total);
                            }

                            result.repo_summary = Some(summary);
                        }
                        Err(e) => {
                            if !self.silent {
                                println!("{} {}", "✗".red(), e);
                            }
                            result.add_error(format!("扫描失败: {}", e));
                        }
                    }
                }

                WorkflowStep::Check {
                    condition,
                    silent: check_silent,
                } => {
                    if !self.silent && !check_silent {
                        print!(
                            "[{}] 检查条件... ",
                            format!("{}/{}", step_num, total_steps).cyan()
                        );
                    }

                    let check_result = self.execute_check(condition, &result);

                    match check_result {
                        Ok(()) => {
                            if !self.silent && !check_silent {
                                println!("{} 通过", "✓".green());
                            }
                        }
                        Err(msg) => {
                            if !self.silent && !check_silent {
                                println!("{} {}", "✗".red(), msg);
                            }
                            result.add_error(msg);
                            result.success = false;
                        }
                    }
                }

                WorkflowStep::PullSafe {
                    jobs,
                    confirm,
                    diff_after,
                } => {
                    let jobs = self.resolve_jobs(*jobs);

                    if !self.silent {
                        println!(
                            "  [{}] 安全 Pull",
                            format!("{}/{}", step_num, total_steps).cyan()
                        );
                    }

                    match self
                        .execute_pull_safe(
                            &db,
                            &sources,
                            jobs,
                            *confirm && !self.dry_run,
                            *diff_after,
                        )
                        .await
                    {
                        Ok(pull_result) => {
                            if !self.silent {
                                if pull_result.total_count == 0 {
                                    println!("  └─ {} 没有需要更新的仓库", "ℹ".blue());
                                } else {
                                    let success_str = pull_result.success_count.to_string().green();
                                    let skip_count = pull_result.skipped_repos.len()
                                        + pull_result.dirty_repos.len();
                                    let skip_str = skip_count.to_string().dimmed();
                                    let failed_str = if pull_result.failed_count > 0 {
                                        pull_result.failed_count.to_string().red()
                                    } else {
                                        pull_result.failed_count.to_string().green()
                                    };
                                    println!(
                                        "  └─ {} 成功: {} | 跳过: {} | 失败: {}",
                                        "▶".blue(),
                                        success_str,
                                        skip_str,
                                        failed_str
                                    );

                                    // 展示成功拉取的仓库列表及最新提交时间
                                    if !pull_result.success_repos.is_empty() {
                                        println!("     {} 成功拉取的仓库:", "✓".green());
                                        for (i, (name, time)) in
                                            pull_result.success_repos.iter().enumerate()
                                        {
                                            let is_last = i == pull_result.success_repos.len() - 1;
                                            let corner = if is_last { "└─" } else { "├─" };
                                            let time_str =
                                                time.as_deref().unwrap_or("（无时间信息）");
                                            println!(
                                                "        {} {} {}",
                                                corner,
                                                name.green(),
                                                format!("- {}", time_str).dimmed()
                                            );
                                        }
                                        println!(); // 空行分隔
                                    }

                                    if !pull_result.dirty_repos.is_empty() {
                                        println!(
                                            "     {} 存在本地变更的仓库（需要手动处理）:",
                                            "⚠️".yellow()
                                        );
                                        println!();

                                        for (i, repo_info) in
                                            pull_result.dirty_repos.iter().enumerate()
                                        {
                                            let is_last = i == pull_result.dirty_repos.len() - 1;
                                            print_repo_change_tree(repo_info, is_last, 8);
                                            if !is_last {
                                                println!();
                                            }
                                        }

                                        println!();
                                        println!("     💡 建议:");
                                        println!(
                                            "        ├─ 运行 'pull-force' 自动 stash → pull → pop"
                                        );
                                        println!(
                                            "        ├─ 运行 'git restore .' 丢弃所有本地变更"
                                        );
                                        println!("        └─ 或手动处理后再运行 'pull-safe'");
                                    }

                                    if *diff_after && !pull_result.pulled_repos.is_empty() {
                                        println!("     {} Pull 后新增提交:", "📋".cyan());
                                        for (name, commits) in &pull_result.pulled_repos {
                                            if !commits.is_empty() {
                                                println!("        {} {}:", "→".cyan(), name.bold());
                                                for commit in commits {
                                                    println!("           {}", commit);
                                                }
                                            }
                                        }
                                    }
                                }
                                println!();
                            }

                            if pull_result.failed_count > 0 {
                                result.success = false;
                            }
                        }
                        Err(e) => {
                            if !self.silent {
                                println!("  └─ {} {}", "✗".red(), e);
                            }
                            result.add_error(format!("安全 Pull 失败: {}", e));
                        }
                    }
                }

                WorkflowStep::PullForce { jobs, diff_after } => {
                    let jobs = self.resolve_jobs(*jobs);

                    if !self.silent {
                        print!(
                            "[{}] 强制 Pull... ",
                            format!("{}/{}", step_num, total_steps).cyan()
                        );
                    }

                    match self
                        .execute_pull_force(&db, &sources, jobs, *diff_after)
                        .await
                    {
                        Ok(pull_result) => {
                            if !self.silent {
                                println!(
                                    "{} {}/{}",
                                    "✓".green(),
                                    pull_result.success_count,
                                    pull_result.total_count
                                );

                                if !pull_result.conflict_repos.is_empty() {
                                    println!(
                                        "   {} 个仓库发生 stash pop 冲突，需要手动恢复:",
                                        pull_result.conflict_repos.len().to_string().yellow()
                                    );
                                    for (i, info) in pull_result.conflict_repos.iter().enumerate() {
                                        let is_last = i == pull_result.conflict_repos.len() - 1;
                                        let repo_connector =
                                            if is_last { "└─" } else { "├─" };

                                        println!("     {} 📦 {}", repo_connector, info.name.bold());

                                        let stash_display = match info.stash_index {
                                            Some(idx) => format!(
                                                "{}（stash@{{{}}}）",
                                                info.stash_message, idx
                                            ),
                                            None => info.stash_message.clone(),
                                        };
                                        println!("        ├─ stash: {}", stash_display);

                                        if !info.conflict_files.is_empty() {
                                            println!(
                                                "        ├─ 冲突文件（{}）:",
                                                info.conflict_files.len()
                                            );
                                            for (j, file) in info.conflict_files.iter().enumerate()
                                            {
                                                let is_last_file =
                                                    j == info.conflict_files.len() - 1;
                                                let file_connector =
                                                    if is_last_file { "└─" } else { "├─" };
                                                println!("        │  {} {}", file_connector, file);
                                            }
                                        }

                                        println!(
                                            "        └─ 恢复命令: git -C {} stash pop stash@{{index}}",
                                            info.path
                                        );
                                    }
                                }
                                if pull_result.failed_count > 0 {
                                    println!(
                                        "   {} 个仓库失败",
                                        pull_result.failed_count.to_string().red()
                                    );
                                }

                                if *diff_after && !pull_result.pulled_repos.is_empty() {
                                    println!("     {} Pull 后新增提交:", "📋".cyan());
                                    for (name, commits) in &pull_result.pulled_repos {
                                        if !commits.is_empty() {
                                            println!("        {} {}:", "→".cyan(), name.bold());
                                            for commit in commits {
                                                println!("           {}", commit);
                                            }
                                        }
                                    }
                                }
                            }

                            if pull_result.has_errors() {
                                result.success = false;
                            }
                        }
                        Err(e) => {
                            if !self.silent {
                                println!("{} {}", "✗".red(), e);
                            }
                            result.add_error(format!("强制 Pull 失败: {}", e));
                        }
                    }
                }

                WorkflowStep::PullBackup { jobs, diff_after } => {
                    let jobs = self.resolve_jobs(*jobs);

                    if !self.silent {
                        println!(
                            "  [{}] 备份 Pull（hard reset 到远程）",
                            format!("{}/{}", step_num, total_steps).cyan()
                        );
                    }

                    match self
                        .execute_pull_backup(&db, &sources, jobs, *diff_after)
                        .await
                    {
                        Ok(pull_result) => {
                            if !self.silent {
                                if pull_result.total_count == 0 {
                                    println!("  └─ {} 没有需要同步或清理的仓库", "ℹ".blue());
                                } else {
                                    println!(
                                        "  └─ {} 成功: {} | 失败: {}",
                                        "▶".blue(),
                                        pull_result.success_count.to_string().green(),
                                        if pull_result.failed_count > 0 {
                                            pull_result.failed_count.to_string().red()
                                        } else {
                                            pull_result.failed_count.to_string().green()
                                        }
                                    );

                                    if !pull_result.archived_repos.is_empty() {
                                        println!(
                                            "     {} 已归档历史（远程历史被重写）:",
                                            "📦".cyan()
                                        );
                                        for (i, (name, archive_ref)) in
                                            pull_result.archived_repos.iter().enumerate()
                                        {
                                            let is_last = i == pull_result.archived_repos.len() - 1;
                                            let corner = if is_last { "└─" } else { "├─" };
                                            println!(
                                                "        {} {} {}",
                                                corner,
                                                name.yellow(),
                                                format!("→ {}", archive_ref).dimmed()
                                            );
                                        }
                                        println!();
                                    }

                                    if !pull_result.success_repos.is_empty() {
                                        println!("     {} 成功同步的仓库:", "✓".green());
                                        for (i, (name, time)) in
                                            pull_result.success_repos.iter().enumerate()
                                        {
                                            let is_last = i == pull_result.success_repos.len() - 1;
                                            let corner = if is_last { "└─" } else { "├─" };
                                            let time_str =
                                                time.as_deref().unwrap_or("（无时间信息）");
                                            println!(
                                                "        {} {} {}",
                                                corner,
                                                name.green(),
                                                format!("- {}", time_str).dimmed()
                                            );
                                        }
                                        println!();
                                    }

                                    if !pull_result.conflict_repos.is_empty() {
                                        println!(
                                            "     {} 个仓库发生 stash pop 冲突，需要手动恢复:",
                                            pull_result.conflict_repos.len().to_string().yellow()
                                        );
                                        for (i, info) in
                                            pull_result.conflict_repos.iter().enumerate()
                                        {
                                            let is_last = i == pull_result.conflict_repos.len() - 1;
                                            let repo_connector =
                                                if is_last { "└─" } else { "├─" };

                                            println!(
                                                "        {} 📦 {}",
                                                repo_connector,
                                                info.name.bold()
                                            );

                                            let stash_display = match info.stash_index {
                                                Some(idx) => format!(
                                                    "{}（stash@{{{}}}）",
                                                    info.stash_message, idx
                                                ),
                                                None => info.stash_message.clone(),
                                            };
                                            println!("           ├─ stash: {}", stash_display);

                                            if !info.conflict_files.is_empty() {
                                                println!(
                                                    "           ├─ 冲突文件（{}）:",
                                                    info.conflict_files.len()
                                                );
                                                for (j, file) in
                                                    info.conflict_files.iter().enumerate()
                                                {
                                                    let is_last_file =
                                                        j == info.conflict_files.len() - 1;
                                                    let file_connector = if is_last_file {
                                                        "└─"
                                                    } else {
                                                        "├─"
                                                    };
                                                    println!(
                                                        "           │  {} {}",
                                                        file_connector, file
                                                    );
                                                }
                                            }

                                            println!(
                                                "           └─ 恢复命令: git -C {} stash pop stash@{{index}}",
                                                info.path
                                            );
                                        }
                                    }

                                    if pull_result.failed_count > 0 {
                                        println!(
                                            "     {} 个仓库失败",
                                            pull_result.failed_count.to_string().red()
                                        );
                                    }

                                    if *diff_after && !pull_result.pulled_repos.is_empty() {
                                        println!("     {} 同步后新增提交:", "📋".cyan());
                                        for (name, commits) in &pull_result.pulled_repos {
                                            if !commits.is_empty() {
                                                println!("        {} {}:", "→".cyan(), name.bold());
                                                for commit in commits {
                                                    println!("           {}", commit);
                                                }
                                            }
                                        }
                                    }
                                }
                                println!();
                            }

                            if pull_result.has_errors() {
                                for (name, path, error) in &pull_result.failed_repos {
                                    result.add_repository_issue(
                                        super::types::RepositoryExecutionIssue {
                                            name: name.clone(),
                                            path: path.clone(),
                                            message: error.clone(),
                                        },
                                    );
                                }
                                for conflict in &pull_result.conflict_repos {
                                    let files = if conflict.conflict_files.is_empty() {
                                        "恢复本地修改时发生冲突".to_string()
                                    } else {
                                        format!(
                                            "恢复本地修改时发生冲突：{}",
                                            conflict.conflict_files.join("、")
                                        )
                                    };
                                    result.add_repository_issue(
                                        super::types::RepositoryExecutionIssue {
                                            name: conflict.name.clone(),
                                            path: conflict.path.clone(),
                                            message: files,
                                        },
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            if !self.silent {
                                println!("  └─ {} {}", "✗".red(), e);
                            }
                            result.add_error(format!("备份 Pull 失败: {}", e));
                        }
                    }
                }
            }
        }

        let duration = start.elapsed();

        if !self.silent {
            println!();
            let status = if result.success {
                format!("{} 已完成", "✓".green())
            } else {
                format!("{} 已完成但存在错误", "⚠".yellow())
            };
            let time_info = format!("耗时 {:.1} 秒", duration.as_secs_f32());
            println!("┌────────────────────────────────────────────────────────────┐");
            println!("│ {:<38} {:>17} │", status, time_info.dimmed());
            println!("└────────────────────────────────────────────────────────────┘");
        }

        Ok(result)
    }

    /// Execute the fetch step
    async fn execute_fetch(
        &self,
        db: &Database,
        sources: &[crate::models::ScanSource],
        jobs: usize,
        timeout: u64,
    ) -> Result<FetchSummary> {
        // 限定范围来自 TUI 已刷新过的异常列表，不需要为了发现新仓库再次扫描
        // 整个根目录。普通工作流仍保留 auto-sync 行为，防止配置源新增仓库后
        // fetch 阶段看不到最新数据库记录。
        if !self.has_target_scope() {
            let app_config = AppConfig::load()?;
            let sync = crate::sync::RepoSync::new(app_config.sync.auto_sync);
            let sync_status = sync.ensure_synced(sources, db, !self.silent).await?;

            if !self.silent && sync_status.needs_scan() {
                println!("  ├─ {}\n", sync_status.description());
            }
        }

        let mut repos = self.repositories_for_scope(db, sources)?;

        if repos.is_empty() {
            if self.has_target_scope() {
                anyhow::bail!("限定范围内未找到仓库");
            }

            let _ = Scanner::scan_all(sources, db, false, jobs).await?;

            repos = self.repositories_for_scope(db, sources)?;
        }
        if repos.is_empty() {
            anyhow::bail!("未找到仓库");
        }

        let fetcher = Fetcher::new(jobs, timeout)
            .with_security_scan(self.security_check)
            .with_auto_skip_high_risk(self.auto_skip_high_risk)
            .with_auto_allow_high_risk(self.auto_allow_high_risk)
            .with_proxy(self.proxy.clone())
            .with_move_to_needauth(true)
            .with_auto_sync(false); // Already manually synced
        let fetcher = if let Some(checker) = &self.cancellation_checker {
            fetcher.with_cancellation_checker(checker.clone())
        } else {
            fetcher
        };
        let fetcher = if let Some(observer) = &self.progress_observer {
            fetcher.with_progress_observer(observer.clone())
        } else {
            fetcher
        };
        fetcher.fetch_and_update(&repos, db, !self.silent).await
    }

    /// Execute the scan step
    async fn execute_scan(
        &self,
        db: &Database,
        sources: &[crate::models::ScanSource],
        output: OutputFormat,
        open: bool,
        only_dirty_or_behind: bool,
    ) -> Result<RepoSummary> {
        use crate::reporter::{
            Reporter, html::HtmlReporter, markdown::MarkdownReporter, save_report_async,
            terminal::TerminalReporter,
        };

        let repos = if self.has_target_scope() {
            self.scan_repositories_for_scope(db, sources).await?
        } else {
            Scanner::scan_all(
                sources,
                db,
                false,
                crate::utils::DEFAULT_MAX_CONCURRENT_SCAN,
            )
            .await?
        };

        if repos.is_empty() {
            if self.has_target_scope() {
                anyhow::bail!("限定范围内未找到 Git 仓库");
            } else {
                anyhow::bail!("未找到 Git 仓库");
            }
        }

        let filtered_repos: Vec<_> = if only_dirty_or_behind {
            repos
                .iter()
                .filter(|r| r.freshness == Freshness::HasUpdates || r.dirty)
                .cloned()
                .collect()
        } else {
            repos.clone()
        };

        let report_repos = if only_dirty_or_behind {
            &filtered_repos
        } else {
            &repos
        };

        let mut summary = RepoSummary::new();
        for repo in report_repos {
            summary.add(repo);
        }

        match output {
            OutputFormat::Terminal => {
                let reporter = TerminalReporter::new();
                let report = reporter.generate(report_repos, &summary)?;
                if !self.silent {
                    println!();
                    println!("{}", report);
                }
            }
            OutputFormat::Html => {
                let reporter = HtmlReporter::new();
                let report = reporter.generate(report_repos, &summary)?;
                let path = save_report_async(report, None, "html".to_string()).await?;

                if !self.silent {
                    println!();
                    println!("{} HTML 报告: {}", "✓".green(), path.display());
                }

                if open {
                    super::types::open_report(&path)?;
                }
            }
            OutputFormat::Markdown => {
                let reporter = MarkdownReporter::new();
                let report = reporter.generate(report_repos, &summary)?;
                let path = save_report_async(report, None, "md".to_string()).await?;
                if !self.silent {
                    println!();
                    println!("{} Markdown 报告: {}", "✓".green(), path.display());
                }
            }
        }

        Ok(summary)
    }

    /// 只刷新限定范围内的仓库状态。
    ///
    /// TUI 选择菜单 3 时，用户已经在界面上看到了明确的异常仓库集合。这里不再
    /// 调用 `Scanner::scan_all()` 遍历整个扫描源，而是对目标仓库逐个执行
    /// `GitOps::inspect()`：成功则写回数据库，失败或路径不存在则保留旧快照。
    /// 这样报告和后续 pull 阶段仍能展示异常记录，同时不会把 341 个仓库重新纳入
    /// 本次同步流程。
    async fn scan_repositories_for_scope(
        &self,
        db: &Database,
        sources: &[crate::models::ScanSource],
    ) -> Result<Vec<crate::models::Repository>> {
        let stored_repos = self.repositories_for_scope(db, sources)?;
        if stored_repos.is_empty() {
            return Ok(Vec::new());
        }

        let total = stored_repos.len();
        if let Some(observer) = &self.phase_progress_observer {
            observer("正在复检目标仓库", 0, total);
        }
        let completed = Arc::new(AtomicUsize::new(0));
        let progress_observer = self.phase_progress_observer.clone();
        let tasks: Vec<_> = stored_repos
            .into_iter()
            .map(|stored| {
                let completed = Arc::clone(&completed);
                let progress_observer = progress_observer.clone();
                move || {
                    let path = std::path::PathBuf::from(&stored.path);
                    let outcome = if !path.exists() {
                        ScopedScanOutcome::Stale {
                            repo: stored,
                            reason: "路径不存在，保留数据库中的最后快照".to_string(),
                        }
                    } else {
                        match crate::git::GitOps::inspect(&path, &stored.root_path) {
                            Ok(mut refreshed) => {
                                refreshed.id = stored.id;
                                refreshed.last_fetch_at = stored.last_fetch_at;
                                refreshed.last_pull_at = stored.last_pull_at;
                                ScopedScanOutcome::Fresh(refreshed)
                            }
                            Err(e) => ScopedScanOutcome::Stale {
                                repo: stored,
                                reason: format!("重新检查失败：{e}"),
                            },
                        }
                    };
                    let current = completed.fetch_add(1, Ordering::AcqRel) + 1;
                    if let Some(observer) = progress_observer {
                        observer("正在复检目标仓库", current, total);
                    }
                    outcome
                }
            })
            .collect();

        let results = crate::concurrent::execute_concurrent_raw(
            tasks,
            crate::utils::DEFAULT_MAX_CONCURRENT_SCAN,
        );
        let mut repos = Vec::new();
        let mut stale_count = 0usize;

        for result in results {
            match result {
                Some(ScopedScanOutcome::Fresh(mut repo)) => {
                    if let Err(e) = db.upsert_repository(&mut repo) {
                        eprintln!(
                            "   {} 限定扫描写入失败 {}: {}",
                            "⚠".yellow(),
                            crate::utils::sanitize_path(&repo.path),
                            e
                        );
                    }
                    repos.push(repo);
                }
                Some(ScopedScanOutcome::Stale { repo, reason }) => {
                    stale_count += 1;
                    if !self.silent {
                        eprintln!(
                            "   {} 限定扫描保留旧快照 {}: {}",
                            "⚠".yellow(),
                            crate::utils::sanitize_path(&repo.path),
                            reason
                        );
                    }
                    repos.push(repo);
                }
                None => {
                    stale_count += 1;
                    if !self.silent {
                        eprintln!("   {} 限定扫描任务 panic，已跳过一条记录", "⚠".yellow());
                    }
                }
            }
        }

        if stale_count > 0 && !self.silent {
            eprintln!(
                "   {} 限定扫描有 {} 条记录无法刷新，报告中会保留旧快照",
                "⚠".yellow(),
                stale_count
            );
        }

        Ok(repos)
    }

    /// Execute the check step
    fn execute_check(&self, condition: &Condition, result: &WorkflowResult) -> Result<(), String> {
        let summary = match &result.repo_summary {
            Some(s) => s,
            None => return Err("没有可用于检查的扫描结果".to_string()),
        };

        match condition {
            Condition::HasBehind => {
                if summary.has_updates > 0 {
                    Err(format!("{} 个仓库落后于远程", summary.has_updates))
                } else {
                    Ok(())
                }
            }
            Condition::HasDirty => {
                if summary.dirty > 0 {
                    Err(format!("{} 个仓库存在本地变更", summary.dirty))
                } else {
                    Ok(())
                }
            }
            Condition::HasError => {
                if summary.unreachable > 0 {
                    Err(format!("{} 个仓库远程不可达", summary.unreachable))
                } else {
                    Ok(())
                }
            }
            Condition::AllSynced => {
                if summary.has_updates == 0 && summary.dirty == 0 && summary.unreachable == 0 {
                    Ok(())
                } else {
                    Err("并非所有仓库都已同步".to_string())
                }
            }
        }
    }

    /// 在 pull/reset 前执行真实远程差异安全扫描。
    ///
    /// fetch 之前本地还没有最新远程对象，无法可靠分析敏感文件、文件数量变化和未知提交者。
    /// 因此这里在 workflow 的 fetch + scan 之后、实际 merge/reset 之前比较 `HEAD`
    /// 与 upstream tracking ref，发现高风险时默认跳过，避免风险提交进入工作区。
    async fn filter_repos_by_pull_security(
        &self,
        repos: Vec<crate::models::Repository>,
        jobs: usize,
    ) -> Result<(Vec<crate::models::Repository>, Vec<String>)> {
        use std::io::{IsTerminal, Write};

        if !self.security_check || repos.is_empty() {
            return Ok((repos, Vec::new()));
        }

        let mut allowed = Vec::new();
        let mut risky = Vec::new();
        let mut skipped = Vec::new();

        if !self.silent {
            println!("  ├─ {} 正在执行 Pull 前安全扫描...", "🛡️".blue());
        }

        let total = repos.len();
        if let Some(observer) = &self.phase_progress_observer {
            observer("正在执行安全扫描", 0, total);
        }
        let completed = Arc::new(AtomicUsize::new(0));
        let cancellation_checker = self.cancellation_checker.clone();
        let progress_observer = self.phase_progress_observer.clone();
        let concurrency = jobs.clamp(1, total);
        let mut scan_results = futures::stream::iter(repos.into_iter().enumerate())
            .map(|(index, repo)| {
                let completed = Arc::clone(&completed);
                let cancellation_checker = cancellation_checker.clone();
                let progress_observer = progress_observer.clone();
                async move {
                    let cancelled = crate::signal_handler::is_shutdown_requested()
                        || cancellation_checker
                            .as_ref()
                            .is_some_and(|checker| checker());
                    let scan_result = if cancelled {
                        Err(anyhow::anyhow!("用户中断，停止安全扫描"))
                    } else {
                        let path = std::path::PathBuf::from(&repo.path);
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(30),
                            tokio::task::spawn_blocking(move || Self::scan_repo_before_pull(&path)),
                        )
                        .await
                        {
                            Ok(Ok(Ok(result))) => Ok(result),
                            Ok(Ok(Err(e))) => Err(e),
                            Ok(Err(_)) => Err(anyhow::anyhow!("安全扫描任务 panic")),
                            Err(_) => Err(anyhow::anyhow!("安全扫描超时（30 秒）")),
                        }
                    };
                    let current = completed.fetch_add(1, Ordering::AcqRel) + 1;
                    if let Some(observer) = progress_observer {
                        observer("正在执行安全扫描", current, total);
                    }
                    (index, repo, scan_result)
                }
            })
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await;
        scan_results.sort_unstable_by_key(|(index, _, _)| *index);
        if self.is_cancelled() {
            anyhow::bail!("用户中断，停止安全扫描");
        }

        for (_, repo, scan_result) in scan_results {
            match scan_result {
                Ok((true, report)) => {
                    if !self.silent && !report.is_empty() && report.contains("安全警告") {
                        println!("{report}");
                    }
                    allowed.push(repo);
                }
                Ok((false, report)) => risky.push((repo, report)),
                Err(e) => {
                    if !self.silent {
                        eprintln!(
                            "  │  {} 安全扫描失败，已跳过 '{}': {}",
                            "⚠".yellow(),
                            repo.name,
                            e
                        );
                    }
                    skipped.push(repo.name);
                }
            }
        }

        if risky.is_empty() {
            return Ok((allowed, skipped));
        }

        if self.auto_allow_high_risk {
            for (repo, _) in risky {
                if !self.silent {
                    println!("  │  {} 已自动放行高风险仓库: {}", "⚠".yellow(), repo.name);
                }
                allowed.push(repo);
            }
            return Ok((allowed, skipped));
        }

        if self.auto_skip_high_risk || self.silent {
            for (repo, _) in risky {
                if !self.silent {
                    println!("  │  {} 已跳过高风险仓库: {}", "⚠".yellow(), repo.name);
                }
                skipped.push(repo.name);
            }
            return Ok((allowed, skipped));
        }

        if !std::io::stdin().is_terminal() {
            for (repo, _) in risky {
                println!(
                    "  │  {} stdin 不是 TTY，已跳过高风险仓库: {}",
                    "⚠".yellow(),
                    repo.name
                );
                skipped.push(repo.name);
            }
            return Ok((allowed, skipped));
        }

        println!();
        println!("{}", "🛡️ Pull 前安全扫描命中高风险仓库".yellow().bold());
        println!("{}", "═".repeat(50).yellow());
        println!(
            "输入 {} 表示全部继续；输入序号（如 {}）只继续指定仓库；直接回车表示全部跳过。",
            "0".green().bold(),
            "1,3,5".green()
        );
        println!("继续的仓库会执行本次 Pull/reset；未选择的仓库会保持当前本地状态。");

        for (index, (repo, report)) in risky.iter().enumerate() {
            println!();
            println!("[{}] {}", index + 1, repo.name.as_str().bold());
            if !report.is_empty() {
                println!("{report}");
            }
        }

        let selection = loop {
            print!("\n请选择要继续 Pull 的高风险仓库序号（0=全部，回车=全部跳过）: ");
            std::io::stdout().flush()?;

            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            match parse_batch_risk_selection(&input, risky.len()) {
                Ok(selection) => break selection,
                Err(e) => {
                    println!("  │  {} {}，请重新输入。", "⚠".yellow(), e);
                }
            }
        };

        for (index, (repo, _)) in risky.into_iter().enumerate() {
            let number = index + 1;
            let continue_pull = match &selection {
                BatchRiskSelection::All => true,
                BatchRiskSelection::Some(selected) => selected.contains(&number),
                BatchRiskSelection::None => false,
            };

            if continue_pull {
                if !self.silent {
                    println!(
                        "  │  {} 将继续 Pull 高风险仓库 [{}]: {}",
                        "⚠".yellow(),
                        number,
                        repo.name
                    );
                }
                allowed.push(repo);
            } else {
                if !self.silent {
                    println!(
                        "  │  {} 已跳过高风险仓库 [{}]: {}",
                        "⚠".yellow(),
                        number,
                        repo.name
                    );
                }
                skipped.push(repo.name);
            }
        }

        Ok((allowed, skipped))
    }

    /// 扫描单个仓库的 HEAD -> upstream tracking ref 差异。
    ///
    /// 返回 `(is_safe, report)`；没有 upstream 或没有目标提交时按安全处理，
    /// 因为这类仓库不会进入 behind pull 流程。
    fn scan_repo_before_pull(path: &std::path::Path) -> Result<(bool, String)> {
        let repo = git2::Repository::open(path)?;
        let local_oid = repo.head().ok().and_then(|head| head.target());
        let remote_oid = Self::resolve_upstream_oid(&repo);

        let (Some(local_oid), Some(remote_oid)) = (local_oid, remote_oid) else {
            return Ok((true, String::new()));
        };

        if local_oid == remote_oid {
            return Ok((true, String::new()));
        }

        let result = SecurityScanner::scan_before_fetch(path, Some(local_oid), Some(remote_oid))?;
        let report = format_security_report(&result);
        Ok((result.is_safe, report))
    }

    /// 解析当前分支的 upstream tracking ref OID。
    fn resolve_upstream_oid(repo: &git2::Repository) -> Option<git2::Oid> {
        let head = repo.head().ok()?;
        let branch_name = head.shorthand().ok()?;
        let branch = repo
            .find_branch(branch_name, git2::BranchType::Local)
            .ok()?;
        let upstream = branch.upstream().ok()?;
        upstream.get().target()
    }

    /// Execute safe pull (clean repositories only)
    #[allow(clippy::type_complexity)]
    async fn execute_pull_safe(
        &self,
        db: &Database,
        sources: &[crate::models::ScanSource],
        jobs: usize,
        confirm: bool,
        diff_after: bool,
    ) -> Result<PullSafeResult> {
        // Concurrency control uses standard library synchronization primitives

        let repos = self.repositories_for_scope(db, sources)?;
        if repos.is_empty() {
            if self.has_target_scope() {
                anyhow::bail!("限定范围内未找到仓库");
            } else {
                anyhow::bail!("未找到仓库");
            }
        }

        let (behind_repos, up_to_date_repos): (Vec<_>, Vec<_>) = repos
            .into_iter()
            .partition(|r| r.freshness == Freshness::HasUpdates);

        if behind_repos.is_empty() {
            let mut result = PullSafeResult::new();
            result.skipped_repos = up_to_date_repos.into_iter().map(|r| r.name).collect();
            return Ok(result);
        }

        let mut clean_repos = Vec::new();
        let mut dirty_repos = Vec::new();

        for repo in behind_repos {
            if repo.dirty {
                dirty_repos.push(repo);
            } else {
                clean_repos.push(repo);
            }
        }

        if clean_repos.is_empty() {
            if !self.silent {
                println!();
                println!("{} 所有落后远程的仓库都有本地变更，已跳过", "⚠".yellow());
                println!();
                println!("{} 变更仓库详情:", "📋".cyan());
                println!();

                // Show tree hierarchy
                for (i, repo_info) in dirty_repos.iter().enumerate() {
                    let is_last = i == dirty_repos.len() - 1;
                    print_repo_change_tree(repo_info, is_last, 3);
                    if !is_last {
                        println!();
                    }
                }

                println!();
                println!("💡 建议:");
                println!("   ├─ 运行 'pull-force' 自动 stash → pull → pop");
                println!("   ├─ 运行 'git restore .' 丢弃所有本地变更");
                println!("   └─ 或手动处理后再运行 'pull-safe'");
            }
            let mut result = PullSafeResult::new();
            result.dirty_repos = dirty_repos.into_iter().map(repo_to_dirty_info).collect();
            return Ok(result);
        }

        // Pull safety check (prevents repo deletion)
        let mut unsafe_repos: Vec<(crate::models::Repository, crate::git::PullSafetyReport)> =
            Vec::new();

        if self.pull_safety_check {
            if !self.silent && !self.dry_run {
                println!("  ├─ {} 正在检查 Pull 安全性...", "🔒".blue());
            }

            for repo in &clean_repos {
                if self.is_cancelled() {
                    anyhow::bail!("用户中断，停止 Pull 操作");
                }

                let path = std::path::PathBuf::from(&repo.path);
                let repo = repo.clone();
                let result = match tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    tokio::task::spawn_blocking(move || {
                        crate::git::GitOps::check_pull_safety(&path)
                    }),
                )
                .await
                {
                    Ok(Ok(Ok(report))) => Ok(report),
                    Ok(Ok(Err(e))) => Err(e),
                    Ok(Err(_)) => Err(crate::error::GetLatestRepoError::Other(anyhow::anyhow!(
                        "安全检查任务 panic"
                    ))),
                    Err(_) => Err(crate::error::GetLatestRepoError::Other(anyhow::anyhow!(
                        "安全检查超时（30 秒）"
                    ))),
                };
                match result {
                    Ok(report) => {
                        if !report.is_safe {
                            unsafe_repos.push((repo, report));
                        }
                    }
                    Err(e) => {
                        unsafe_repos.push((
                            repo,
                            crate::git::PullSafetyReport {
                                is_safe: false,
                                remote_commits: 0,
                                previous_remote_commits: 0,
                                change_ratio: 0.0,
                                warning: Some(format!("安全检查失败: {}", e)),
                                details: vec![],
                            },
                        ));
                    }
                }
            }

            if !unsafe_repos.is_empty() {
                let unsafe_names: std::collections::HashSet<_> =
                    unsafe_repos.iter().map(|(r, _)| r.name.clone()).collect();
                clean_repos.retain(|r| !unsafe_names.contains(&r.name));

                if !self.silent {
                    println!("  │");
                    println!(
                        "  ├─ {} 跳过 {} 个高风险仓库:",
                        "🚨".red(),
                        unsafe_repos.len()
                    );
                    for (repo, report) in &unsafe_repos {
                        if let Some(ref warning) = report.warning {
                            println!("  │    ⚠ {}: {}", repo.name.red().bold(), warning);
                        } else {
                            println!("  │    ⚠ {}", repo.name.red().bold());
                        }
                    }

                    if clean_repos.is_empty() {
                        println!("  │");
                        println!(
                            "  └─ {}",
                            "所有落后远程的仓库都有风险或本地变更，无法安全 Pull".yellow()
                        );
                        let mut result = PullSafeResult::new();
                        result.dirty_repos =
                            dirty_repos.into_iter().map(repo_to_dirty_info).collect();
                        return Ok(result);
                    }

                    println!("  │");
                    println!(
                        "  ├─ {} {} 个安全仓库将继续 Pull",
                        "✓".green(),
                        clean_repos.len()
                    );
                } else if clean_repos.is_empty() {
                    let mut result = PullSafeResult::new();
                    result.dirty_repos = dirty_repos.into_iter().map(repo_to_dirty_info).collect();
                    return Ok(result);
                }
            }
        }

        let mut security_skipped_repos = Vec::new();
        if !self.dry_run {
            let (filtered, skipped) = self
                .filter_repos_by_pull_security(clean_repos, jobs)
                .await?;
            clean_repos = filtered;
            security_skipped_repos = skipped;

            if clean_repos.is_empty() {
                let mut result = PullSafeResult::new();
                result.dirty_repos = dirty_repos.into_iter().map(repo_to_dirty_info).collect();
                result.skipped_repos = up_to_date_repos
                    .into_iter()
                    .map(|r| r.name)
                    .chain(security_skipped_repos)
                    .collect();
                return Ok(result);
            }
        }

        // Dry-run preview
        if self.dry_run {
            if !self.silent {
                println!();
                println!("  ┌─ {} Dry-run 预览 ─────────────────────", "📋".cyan());

                if !dirty_repos.is_empty() {
                    println!("  │");
                    println!("  │ {} 将跳过的仓库（存在本地变更）:", "○".dimmed());
                    println!("  │");

                    for (i, repo) in dirty_repos.iter().enumerate() {
                        let is_last = i == dirty_repos.len() - 1;
                        let repo_connector = if is_last {
                            "  │   └─"
                        } else {
                            "  │   ├─"
                        };

                        println!("{} 📦 {}", repo_connector, repo.name.dimmed());

                        let meta_connector = if is_last {
                            "  │       "
                        } else {
                            "  │   │   "
                        };
                        let branch_info = repo.branch.as_deref().unwrap_or("未知");
                        println!(
                            "{}{} [{}]（{} 个文件）",
                            meta_connector,
                            "🌿".dimmed(),
                            branch_info.dimmed(),
                            repo.file_changes.len()
                        );

                        // Show the first few changed files
                        for (j, change) in repo.file_changes.iter().take(2).enumerate() {
                            let is_last_file = is_last
                                && j == repo.file_changes.len().min(2) - 1
                                && repo.file_changes.len() <= 2;
                            let file_connector = if is_last_file {
                                "  │           └─"
                            } else {
                                "  │           ├─"
                            };

                            let status_icon = match change.status.as_str() {
                                "added" => "✚",
                                "deleted" => "✗",
                                "modified" => "✎",
                                "renamed" => "➜",
                                _ => "?",
                            };

                            println!(
                                "{} {} {}",
                                file_connector,
                                status_icon,
                                change.path.dimmed()
                            );
                        }

                        if repo.file_changes.len() > 2 {
                            let more_connector = if is_last {
                                "  │           └─"
                            } else {
                                "  │           ├─"
                            };
                            println!(
                                "{} ... 以及 {} 个文件",
                                more_connector,
                                repo.file_changes.len() - 2
                            );
                        }
                    }
                }

                if !unsafe_repos.is_empty() {
                    println!("  │");
                    println!("  │ {} 将阻止的仓库（检测到删除风险）:", "🚨".red());
                    for (repo, _) in &unsafe_repos {
                        println!("  │   • {}", repo.name.red());
                    }
                }

                if !clean_repos.is_empty() {
                    println!("  │");
                    println!("  │ {} 将更新的仓库（安全）:", "▶".green());
                    for repo in &clean_repos {
                        println!(
                            "  │   • {}（落后 {} 个提交）",
                            repo.name.green(),
                            repo.behind_count.to_string().yellow()
                        );
                    }
                }

                println!("  │");
                println!("  └─ {} 预览完成，未实际执行任何操作", "ℹ".blue());
            }

            let mut result = PullSafeResult::new();
            result.dirty_repos = dirty_repos.into_iter().map(repo_to_dirty_info).collect();
            result.skipped_repos = up_to_date_repos.into_iter().map(|r| r.name).collect();
            return Ok(result);
        }

        // 静默模式只控制输出，不能隐式授予修改仓库的权限。
        validate_pull_confirmation_mode(confirm, self.silent)?;

        // Confirmation prompt
        if confirm && !clean_repos.is_empty() {
            println!();
            println!(
                "{} 将更新以下 {} 个干净仓库:",
                "▶".cyan(),
                clean_repos.len()
            );
            for repo in &clean_repos {
                println!("   - {}（落后 {} 个提交）", repo.name, repo.behind_count);
            }
            if !dirty_repos.is_empty() {
                println!();
                println!(
                    "{} 以下 {} 个仓库存在本地变更，将被跳过:",
                    "!".yellow(),
                    dirty_repos.len()
                );
                println!();

                for (i, repo_info) in dirty_repos.iter().enumerate() {
                    let is_last = i == dirty_repos.len() - 1;
                    let repo_connector = if is_last { "└─" } else { "├─" };

                    println!("   {} 📦 {}", repo_connector, repo_info.name);

                    let meta_connector = if is_last { "      " } else { "   │  " };
                    let branch_info = repo_info.branch.as_deref().unwrap_or("未知");
                    println!(
                        "{} {} [{}]（{} 个文件）",
                        meta_connector,
                        "🌿".dimmed(),
                        branch_info,
                        repo_info.file_changes.len()
                    );

                    // Show the first 3 changed files
                    for (j, change) in repo_info.file_changes.iter().take(3).enumerate() {
                        let is_last_file = is_last
                            && j == repo_info.file_changes.len().min(3) - 1
                            && repo_info.file_changes.len() <= 3;
                        let file_connector = if is_last_file {
                            "       └─"
                        } else {
                            "       ├─"
                        };

                        let status_icon = match change.status.as_str() {
                            "added" => "✚",
                            "deleted" => "✗",
                            "modified" => "✎",
                            "renamed" => "➜",
                            _ => "?",
                        };

                        println!(
                            "{}{} {} {}",
                            file_connector,
                            status_icon,
                            change.path,
                            if change.staged {
                                "（已暂存）".green()
                            } else {
                                "（未暂存）".dimmed()
                            }
                        );
                    }

                    if repo_info.file_changes.len() > 3 {
                        let more_connector = if is_last {
                            "       └─"
                        } else {
                            "       ├─"
                        };
                        println!(
                            "{} ... 以及 {} 个文件",
                            more_connector,
                            repo_info.file_changes.len() - 3
                        );
                    }

                    if !is_last {
                        println!();
                    }
                }
            }
            use std::io::IsTerminal;
            if !std::io::stdin().is_terminal() {
                anyhow::bail!("stdin 不是 TTY，请使用 --yes 跳过确认");
            }

            print!("\n确认执行？[Y/n] ");
            use std::io::Write;
            std::io::stdout().flush()?;

            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;

            if !input.trim().is_empty() && !input.trim().eq_ignore_ascii_case("y") {
                anyhow::bail!("用户已取消");
            }
        }

        // Concurrent pull using the unified concurrent executor
        // Features:
        // - Auto-handles panics
        // - Uses blocking wait (no busy-wait)
        // - Reasonable timeout
        use crate::concurrent::execute_concurrent_raw;

        // 若启用 diff_after，预先记录 pull 前的 HEAD OID，用于精确显示新增提交
        let mut original_oids: std::collections::HashMap<String, git2::Oid> =
            std::collections::HashMap::new();
        if diff_after {
            for repo in &clean_repos {
                let path = std::path::PathBuf::from(&repo.path);
                if let Ok(repo_git) = git2::Repository::open(&path)
                    && let Ok(head) = repo_git.head()
                    && let Some(oid) = head.target()
                {
                    original_oids.insert(repo.path.clone(), oid);
                }
            }
        }

        // Build the task list
        let tasks: Vec<_> = clean_repos
            .into_iter()
            .map(|repo| {
                let path = std::path::PathBuf::from(&repo.path);
                let name = repo.name.clone();
                let repo_path = repo.path.clone();
                move || {
                    let result = crate::git::GitOps::pull_ff_only(&path);
                    (name, repo_path, result)
                }
            })
            .collect();

        // Execute concurrent tasks
        let results: Vec<Option<(String, String, Result<(), crate::error::GetLatestRepoError>)>> =
            execute_concurrent_raw(tasks, jobs);

        let mut pull_result = PullSafeResult::new();
        pull_result.dirty_repos = dirty_repos.into_iter().map(repo_to_dirty_info).collect();
        pull_result.skipped_repos = up_to_date_repos
            .into_iter()
            .map(|r| r.name)
            .chain(security_skipped_repos)
            .collect();
        let mut success_paths: Vec<(String, String)> = Vec::new();

        // Process results (None means panicked)
        for result in results {
            pull_result.total_count += 1;

            match result {
                Some((name, path, Ok(()))) => {
                    pull_result.success_count += 1;
                    success_paths.push((name.clone(), path.clone()));

                    // Refresh the repository status and collect latest commit time
                    let mut latest_time = None;
                    if !self.defer_status_refresh
                        && let Ok(Some(old_repo)) = db.get_repository(&path)
                    {
                        let path_buf = std::path::PathBuf::from(&path);
                        let root_path = old_repo.root_path.clone();
                        if let Ok(Ok(Ok(mut fresh))) = tokio::time::timeout(
                            std::time::Duration::from_secs(30),
                            tokio::task::spawn_blocking(move || {
                                crate::git::GitOps::inspect(&path_buf, &root_path)
                            }),
                        )
                        .await
                        {
                            fresh.id = old_repo.id;
                            fresh.last_fetch_at = old_repo.last_fetch_at;
                            fresh.last_pull_at = Some(chrono::Local::now());
                            latest_time = fresh.last_commit_at;
                            if let Err(e) = db.upsert_repository(&mut fresh) {
                                eprintln!(
                                    "   ⚠️ 更新仓库状态失败 '{}': {}",
                                    crate::utils::sanitize_path(&path),
                                    e
                                );
                            } else {
                                // Only update pull time after upsert succeeds
                                if let Err(e) = db.update_pull_time(&path) {
                                    eprintln!(
                                        "   ⚠️ 更新 pull 时间失败 '{}': {}",
                                        crate::utils::sanitize_path(&path),
                                        e
                                    );
                                }
                            }
                        }
                    }
                    let time_str = latest_time.map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string());
                    pull_result.success_repos.push((name, time_str));
                }
                Some((name, _, Err(e))) => {
                    pull_result.failed_count += 1;
                    if !self.silent {
                        eprintln!("   {} {} pull 失败: {}", "✗".red(), name, e);
                    }
                }
                None => {
                    pull_result.failed_count += 1;
                    if !self.silent {
                        if self.is_cancelled() {
                            eprintln!("   {} pull 任务被中断信号停止", "⚠️".yellow());
                        } else {
                            eprintln!("   {} pull 任务 panic", "✗".red());
                        }
                    }
                }
            }
        }

        if diff_after && !success_paths.is_empty() {
            for (name, path) in success_paths {
                let path_buf = std::path::PathBuf::from(&path);
                if let Some(&since_oid) = original_oids.get(&path) {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        tokio::task::spawn_blocking(move || {
                            crate::git::GitOps::get_commits_since(&path_buf, since_oid)
                        }),
                    )
                    .await
                    {
                        Ok(Ok(Ok(commits))) => {
                            pull_result.pulled_repos.push((name, commits));
                        }
                        _ => {
                            pull_result
                                .pulled_repos
                                .push((name, vec!["(无法获取新增提交信息)".to_string()]));
                        }
                    }
                } else {
                    // 未记录到原始 OID（极少见），回退到最近 10 条
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        tokio::task::spawn_blocking(move || {
                            crate::git::GitOps::get_recent_commits(&path_buf, 10)
                        }),
                    )
                    .await
                    {
                        Ok(Ok(Ok(commits))) => {
                            pull_result.pulled_repos.push((name, commits));
                        }
                        _ => {
                            pull_result
                                .pulled_repos
                                .push((name, vec!["(无法获取提交信息)".to_string()]));
                        }
                    }
                }
            }
        }

        Ok(pull_result)
    }

    /// Execute backup pull (hard reset to remote, handles diverged history)
    #[allow(clippy::type_complexity)]
    async fn execute_pull_backup(
        &self,
        db: &Database,
        sources: &[crate::models::ScanSource],
        jobs: usize,
        diff_after: bool,
    ) -> Result<super::types::PullBackupResult> {
        use crate::concurrent::execute_concurrent_raw;

        let repos = self.repositories_for_scope(db, sources)?;
        if repos.is_empty() {
            if self.has_target_scope() {
                anyhow::bail!("限定范围内未找到仓库");
            } else {
                anyhow::bail!("未找到仓库");
            }
        }
        let scoped_total = repos.len();

        let mut backup_repos: Vec<_> = repos
            .into_iter()
            .filter(|r| r.freshness == Freshness::HasUpdates || r.dirty)
            .collect();

        if backup_repos.is_empty() {
            if let Some(observer) = &self.progress_observer {
                observer(scoped_total, scoped_total);
            }
            return Ok(super::types::PullBackupResult::new());
        }

        let remote_update_paths: std::collections::HashSet<String> = backup_repos
            .iter()
            .filter(|repo| repo.freshness == Freshness::HasUpdates)
            .map(|repo| repo.path.clone())
            .collect();

        if !remote_update_paths.is_empty() {
            let security_candidates: Vec<_> = backup_repos
                .iter()
                .filter(|repo| remote_update_paths.contains(&repo.path))
                .cloned()
                .collect();
            let (filtered_repos, _security_skipped) = self
                .filter_repos_by_pull_security(security_candidates, jobs)
                .await?;
            let allowed_paths: std::collections::HashSet<String> =
                filtered_repos.into_iter().map(|repo| repo.path).collect();
            backup_repos.retain(|repo| {
                repo.freshness != Freshness::HasUpdates || allowed_paths.contains(&repo.path)
            });
        }

        if backup_repos.is_empty() {
            if let Some(observer) = &self.progress_observer {
                observer(scoped_total, scoped_total);
            }
            return Ok(super::types::PullBackupResult::new());
        }

        // 若启用 diff_after，预先记录 pull 前的 HEAD OID
        let mut original_oids: std::collections::HashMap<String, git2::Oid> =
            std::collections::HashMap::new();
        if diff_after {
            for repo in &backup_repos {
                let path = std::path::PathBuf::from(&repo.path);
                if let Ok(repo_git) = git2::Repository::open(&path)
                    && let Ok(head) = repo_git.head()
                    && let Some(oid) = head.target()
                {
                    original_oids.insert(repo.path.clone(), oid);
                }
            }
        }

        let task_repositories: Vec<_> = backup_repos
            .iter()
            .map(|repo| (repo.name.clone(), repo.path.clone()))
            .collect();
        let no_action_count = scoped_total.saturating_sub(task_repositories.len());
        if let Some(observer) = &self.progress_observer {
            observer(no_action_count, scoped_total);
        }
        let completed_count = Arc::new(AtomicUsize::new(no_action_count));
        let cancellation_checker = self.cancellation_checker.clone();
        let progress_observer = self.progress_observer.clone();
        let tasks: Vec<_> = backup_repos
            .into_iter()
            .map(|repo| {
                let path = std::path::PathBuf::from(&repo.path);
                let name = repo.name.clone();
                let repo_path = repo.path.clone();
                let cancellation_checker = cancellation_checker.clone();
                let progress_observer = progress_observer.clone();
                let completed_count = Arc::clone(&completed_count);
                move || {
                    let result = if let Some(checker) = cancellation_checker {
                        crate::git::GitOps::pull_backup_with_cancellation(&path, checker)
                    } else {
                        crate::git::GitOps::pull_backup(&path)
                    };
                    let completed = completed_count.fetch_add(1, Ordering::AcqRel) + 1;
                    if let Some(observer) = progress_observer {
                        observer(completed, scoped_total);
                    }
                    (name, repo_path, result)
                }
            })
            .collect();

        let results: Vec<
            Option<(
                String,
                String,
                Result<
                    (crate::git::PullForceOutcome, Option<String>),
                    crate::error::GetLatestRepoError,
                >,
            )>,
        > = execute_concurrent_raw(tasks, jobs);

        let mut pull_result = super::types::PullBackupResult::new();
        let mut success_paths: Vec<(String, String)> = Vec::new();

        for ((task_name, task_path), result) in task_repositories.into_iter().zip(results) {
            pull_result.total_count += 1;

            match result {
                Some((name, path, Ok((crate::git::PullForceOutcome::Success, archive_ref)))) => {
                    pull_result.success_count += 1;
                    success_paths.push((name.clone(), path.clone()));

                    if let Some(ref ar) = archive_ref {
                        pull_result.archived_repos.push((name.clone(), ar.clone()));
                    }

                    let mut latest_time = None;
                    if !self.defer_status_refresh
                        && let Ok(Some(old_repo)) = db.get_repository(&path)
                    {
                        let path_buf = std::path::PathBuf::from(&path);
                        let root_path = old_repo.root_path.clone();
                        if let Ok(Ok(Ok(mut fresh))) = tokio::time::timeout(
                            std::time::Duration::from_secs(30),
                            tokio::task::spawn_blocking(move || {
                                crate::git::GitOps::inspect(&path_buf, &root_path)
                            }),
                        )
                        .await
                        {
                            fresh.id = old_repo.id;
                            fresh.last_fetch_at = old_repo.last_fetch_at;
                            fresh.last_pull_at = Some(chrono::Local::now());
                            latest_time = fresh.last_commit_at;
                            if let Err(e) = db.upsert_repository(&mut fresh) {
                                eprintln!(
                                    "   ⚠️ 更新仓库状态失败 '{}': {}",
                                    crate::utils::sanitize_path(&path),
                                    e
                                );
                            } else {
                                if let Err(e) = db.update_pull_time(&path) {
                                    eprintln!(
                                        "   ⚠️ 更新 pull 时间失败 '{}': {}",
                                        crate::utils::sanitize_path(&path),
                                        e
                                    );
                                }
                            }
                        }
                    }
                    let time_str = latest_time.map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string());
                    pull_result.success_repos.push((name, time_str));
                }
                Some((
                    name,
                    path,
                    Ok((
                        crate::git::PullForceOutcome::Conflict {
                            stash_name,
                            conflict_files,
                            stash_index,
                        },
                        archive_ref,
                    )),
                )) => {
                    pull_result.failed_count += 1;
                    if let Some(ref ar) = archive_ref {
                        pull_result.archived_repos.push((name.clone(), ar.clone()));
                    }
                    pull_result.conflict_repos.push(super::types::ConflictInfo {
                        name: name.clone(),
                        path: path.clone(),
                        stash_message: stash_name,
                        conflict_files,
                        stash_index,
                    });
                }
                Some((name, path, Err(e))) => {
                    pull_result.failed_count += 1;
                    pull_result
                        .failed_repos
                        .push((name.clone(), path, e.to_string()));
                    if !self.silent {
                        eprintln!("   {} {} 备份 Pull 失败: {}", "✗".red(), name, e);
                    }
                }
                None => {
                    pull_result.failed_count += 1;
                    let reason = if self.is_cancelled() {
                        "任务被中断信号停止"
                    } else {
                        "任务异常退出"
                    };
                    pull_result
                        .failed_repos
                        .push((task_name, task_path, reason.to_string()));
                    if !self.silent {
                        if self.is_cancelled() {
                            eprintln!("   {} 备份 Pull 任务被中断信号停止", "⚠️".yellow());
                        } else {
                            eprintln!("   {} 备份 Pull 任务 panic", "✗".red());
                        }
                    }
                }
            }
        }

        if !self.defer_status_refresh {
            // CLI 没有后续权威复检，因此需在工作流内刷新冲突仓库。
            for conflict in &pull_result.conflict_repos {
                if let Ok(Some(old_repo)) = db.get_repository(&conflict.path) {
                    let path_buf = std::path::PathBuf::from(&conflict.path);
                    let root_path = old_repo.root_path.clone();
                    if let Ok(Ok(Ok(mut fresh))) = tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        tokio::task::spawn_blocking(move || {
                            crate::git::GitOps::inspect(&path_buf, &root_path)
                        }),
                    )
                    .await
                    {
                        fresh.id = old_repo.id;
                        fresh.last_fetch_at = old_repo.last_fetch_at;
                        fresh.last_pull_at = Some(chrono::Local::now());
                        if let Err(e) = db.upsert_repository(&mut fresh) {
                            eprintln!("   警告：更新冲突仓库状态失败: {}", e);
                        }
                    }
                }
            }
        }

        // diff_after
        if diff_after && !success_paths.is_empty() {
            for (name, path) in success_paths {
                let path_buf = std::path::PathBuf::from(&path);
                if let Some(&since_oid) = original_oids.get(&path) {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        tokio::task::spawn_blocking(move || {
                            crate::git::GitOps::get_commits_since(&path_buf, since_oid)
                        }),
                    )
                    .await
                    {
                        Ok(Ok(Ok(commits))) => {
                            pull_result.pulled_repos.push((name, commits));
                        }
                        _ => {
                            pull_result
                                .pulled_repos
                                .push((name, vec!["(无法获取新增提交信息)".to_string()]));
                        }
                    }
                } else {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        tokio::task::spawn_blocking(move || {
                            crate::git::GitOps::get_recent_commits(&path_buf, 10)
                        }),
                    )
                    .await
                    {
                        Ok(Ok(Ok(commits))) => {
                            pull_result.pulled_repos.push((name, commits));
                        }
                        _ => {
                            pull_result
                                .pulled_repos
                                .push((name, vec!["(无法获取提交信息)".to_string()]));
                        }
                    }
                }
            }
        }

        Ok(pull_result)
    }

    /// Execute force pull
    #[allow(clippy::type_complexity)]
    async fn execute_pull_force(
        &self,
        db: &Database,
        sources: &[crate::models::ScanSource],
        jobs: usize,
        diff_after: bool,
    ) -> Result<PullForceResult> {
        // Concurrency control uses standard library synchronization primitives

        let repos = self.repositories_for_scope(db, sources)?;
        if repos.is_empty() {
            if self.has_target_scope() {
                anyhow::bail!("限定范围内未找到仓库");
            } else {
                anyhow::bail!("未找到仓库");
            }
        }

        let mut behind_repos: Vec<_> = repos
            .into_iter()
            .filter(|r| r.freshness == Freshness::HasUpdates)
            .collect();

        if behind_repos.is_empty() {
            return Ok(PullForceResult::new());
        }

        let (filtered_repos, _security_skipped) = self
            .filter_repos_by_pull_security(behind_repos, jobs)
            .await?;
        behind_repos = filtered_repos;

        if behind_repos.is_empty() {
            return Ok(PullForceResult::new());
        }

        // 若启用 diff_after，预先记录 pull 前的 HEAD OID，用于精确显示新增提交
        let mut original_oids: std::collections::HashMap<String, git2::Oid> =
            std::collections::HashMap::new();
        if diff_after {
            for repo in &behind_repos {
                let path = std::path::PathBuf::from(&repo.path);
                if let Ok(repo_git) = git2::Repository::open(&path)
                    && let Ok(head) = repo_git.head()
                    && let Some(oid) = head.target()
                {
                    original_oids.insert(repo.path.clone(), oid);
                }
            }
        }

        // Concurrent Pull (using unified concurrent executor)
        use crate::concurrent::execute_concurrent_raw;

        // Build the task list
        let tasks: Vec<_> = behind_repos
            .into_iter()
            .map(|repo| {
                let path = std::path::PathBuf::from(&repo.path);
                let name = repo.name.clone();
                let repo_path = repo.path.clone();
                move || {
                    let result = crate::git::GitOps::pull_force(&path);
                    (name, repo_path, result)
                }
            })
            .collect();

        // Execute concurrent tasks
        let results: Vec<
            Option<(
                String,
                String,
                Result<crate::git::PullForceOutcome, crate::error::GetLatestRepoError>,
            )>,
        > = execute_concurrent_raw(tasks, jobs);

        let mut pull_result = PullForceResult::new();
        let mut success_paths: Vec<(String, String)> = Vec::new();

        // Process results (None means panicked)
        for result in results {
            pull_result.total_count += 1;

            match result {
                Some((name, path, Ok(crate::git::PullForceOutcome::Success))) => {
                    pull_result.success_count += 1;
                    success_paths.push((name, path));
                }
                Some((
                    name,
                    path,
                    Ok(crate::git::PullForceOutcome::Conflict {
                        stash_name,
                        conflict_files,
                        stash_index,
                    }),
                )) => {
                    pull_result.failed_count += 1;
                    pull_result
                        .conflict_repos
                        .push(crate::workflow::types::ConflictInfo {
                            name: name.clone(),
                            path: path.clone(),
                            stash_message: stash_name,
                            conflict_files,
                            stash_index,
                        });
                }
                Some((name, _, Err(e))) => {
                    pull_result.failed_count += 1;
                    eprintln!("   {} {} pull 失败: {}", "✗".red(), name, e);
                }
                None => {
                    pull_result.failed_count += 1;
                    if self.is_cancelled() {
                        eprintln!("   {} pull 任务被中断信号停止", "⚠️".yellow());
                    } else {
                        eprintln!("   {} pull 任务 panic", "✗".red());
                    }
                }
            }
        }

        if !self.defer_status_refresh {
            // CLI 没有后续权威复检，因此需在工作流内刷新成功与冲突仓库。
            for (_name, path) in &success_paths {
                if let Ok(Some(old_repo)) = db.get_repository(path) {
                    let path_buf = std::path::PathBuf::from(path);
                    let root_path = old_repo.root_path.clone();
                    if let Ok(Ok(Ok(mut fresh))) = tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        tokio::task::spawn_blocking(move || {
                            crate::git::GitOps::inspect(&path_buf, &root_path)
                        }),
                    )
                    .await
                    {
                        fresh.id = old_repo.id;
                        fresh.last_fetch_at = old_repo.last_fetch_at;
                        fresh.last_pull_at = Some(chrono::Local::now());
                        if let Err(e) = db.upsert_repository(&mut fresh) {
                            eprintln!("   警告：Pull 后更新仓库失败: {}", e);
                        } else if let Err(e) = db.update_pull_time(path) {
                            eprintln!(
                                "   ⚠️ 更新 pull 时间失败 '{}': {}",
                                crate::utils::sanitize_path(path),
                                e
                            );
                        }
                    }
                }
            }

            for conflict in &pull_result.conflict_repos {
                if let Ok(Some(old_repo)) = db.get_repository(&conflict.path) {
                    let path_buf = std::path::PathBuf::from(&conflict.path);
                    let root_path = old_repo.root_path.clone();
                    if let Ok(Ok(Ok(mut fresh))) = tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        tokio::task::spawn_blocking(move || {
                            crate::git::GitOps::inspect(&path_buf, &root_path)
                        }),
                    )
                    .await
                    {
                        fresh.id = old_repo.id;
                        fresh.last_fetch_at = old_repo.last_fetch_at;
                        fresh.last_pull_at = Some(chrono::Local::now());
                        if let Err(e) = db.upsert_repository(&mut fresh) {
                            eprintln!("   警告：更新冲突仓库状态失败: {}", e);
                        }
                    }
                }
            }
        }

        // diff_after: 精确显示本次 pull 新增的提交
        if diff_after && !success_paths.is_empty() {
            for (name, path) in success_paths {
                let path_buf = std::path::PathBuf::from(&path);
                if let Some(&since_oid) = original_oids.get(&path) {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        tokio::task::spawn_blocking(move || {
                            crate::git::GitOps::get_commits_since(&path_buf, since_oid)
                        }),
                    )
                    .await
                    {
                        Ok(Ok(Ok(commits))) => {
                            pull_result.pulled_repos.push((name, commits));
                        }
                        _ => {
                            pull_result
                                .pulled_repos
                                .push((name, vec!["(无法获取新增提交信息)".to_string()]));
                        }
                    }
                } else {
                    // 未记录到原始 OID（极少见），回退到最近 10 条
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        tokio::task::spawn_blocking(move || {
                            crate::git::GitOps::get_recent_commits(&path_buf, 10)
                        }),
                    )
                    .await
                    {
                        Ok(Ok(Ok(commits))) => {
                            pull_result.pulled_repos.push((name, commits));
                        }
                        _ => {
                            pull_result
                                .pulled_repos
                                .push((name, vec!["(无法获取提交信息)".to_string()]));
                        }
                    }
                }
            }
        }

        Ok(pull_result)
    }

    /// Print dry-run plan
    fn print_dry_run(&self) {
        println!("{}", "[Dry Run] 执行计划:".yellow().bold());
        println!();

        for (idx, step) in self.workflow.steps.iter().enumerate() {
            let step_num = idx + 1;
            match step {
                WorkflowStep::Fetch { jobs, timeout } => {
                    let jobs = self.resolve_jobs(*jobs);
                    let timeout = self.resolve_timeout(*timeout);
                    println!("  [{}] Fetch", step_num);
                    println!("      并发: {} | 超时: {} 秒", jobs, timeout);
                }
                WorkflowStep::Scan {
                    output,
                    open,
                    only_dirty_or_behind,
                } => {
                    let output_name = match output {
                        OutputFormat::Terminal => "终端",
                        OutputFormat::Html => "HTML",
                        OutputFormat::Markdown => "Markdown",
                    };
                    println!("  [{}] Scan ({})", step_num, output_name);
                    println!(
                        "      自动打开: {} | 只显示需关注仓库: {}",
                        yes_no(*open),
                        yes_no(*only_dirty_or_behind)
                    );
                }
                WorkflowStep::Check { condition, .. } => {
                    let cond_name = match condition {
                        Condition::HasBehind => "存在落后远程的仓库",
                        Condition::HasDirty => "存在本地变更",
                        Condition::HasError => "存在错误",
                        Condition::AllSynced => "全部已同步",
                    };
                    println!("  [{}] Check ({})", step_num, cond_name);
                }
                WorkflowStep::PullSafe {
                    jobs,
                    confirm,
                    diff_after,
                } => {
                    let jobs = self.resolve_jobs(*jobs);
                    println!("  [{}] PullSafe", step_num);
                    println!("      策略: 只 Pull 干净仓库（ff-only）");
                    println!("      有本地变更的仓库: 跳过并提示");
                    println!("      确认提示: {}", yes_no(*confirm));
                    println!("      显示差异: {}", yes_no(*diff_after));
                    println!("      并发: {}", jobs);
                }
                WorkflowStep::PullForce { jobs, diff_after } => {
                    let jobs = self.resolve_jobs(*jobs);
                    println!("  [{}] PullForce", step_num);
                    println!("      流程: stash → pull --ff-only → stash pop");
                    println!("      显示差异: {}", yes_no(*diff_after));
                    println!("      并发: {}", jobs);
                    println!("      冲突处理: 停止并提示手动解决");
                }
                WorkflowStep::PullBackup { jobs, diff_after } => {
                    let jobs = self.resolve_jobs(*jobs);
                    println!("  [{}] PullBackup", step_num);
                    println!(
                        "      流程: stash 保护本地变更（不自动 pop）→ git reset --hard origin/<branch>"
                    );
                    println!("      策略: 严格镜像远程，可处理 force-push / rebase");
                    println!("      显示差异: {}", yes_no(*diff_after));
                    println!("      并发: {}", jobs);
                    println!("      恢复方式: 如需找回本地修改，手动查看 git stash list");
                }
            }
            println!();
        }

        println!("{}", "参数覆盖:".dimmed());
        println!(
            "  并发: {}（默认: {}）",
            self.resolve_jobs(None),
            self.workflow.default_jobs
        );
        println!(
            "  超时: {} 秒（默认: {} 秒）",
            self.resolve_timeout(None),
            self.workflow.default_timeout
        );
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "是" } else { "否" }
}

/// 确认策略与展示策略必须解耦；静默执行需要调用方显式传入 `--yes`。
fn validate_pull_confirmation_mode(confirm: bool, silent: bool) -> Result<()> {
    if confirm && silent {
        anyhow::bail!("静默模式不会自动确认 Pull，请显式使用 --yes 授权执行");
    }
    Ok(())
}

fn workflow_steps_modify_repositories(steps: &[WorkflowStep]) -> bool {
    steps.iter().any(|step| {
        matches!(
            step,
            WorkflowStep::PullSafe { .. }
                | WorkflowStep::PullForce { .. }
                | WorkflowStep::PullBackup { .. }
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scoped_repo(
        id: Option<i64>,
        name: &str,
        path: &str,
        root_path: &str,
        upstream_url: Option<&str>,
    ) -> crate::models::Repository {
        crate::models::Repository {
            id,
            name: name.to_string(),
            path: path.to_string(),
            root_path: root_path.to_string(),
            upstream_url: upstream_url.map(str::to_string),
            ..crate::models::Repository::default()
        }
    }

    #[test]
    fn repository_scope_matches_original_path_and_upstream_identity() {
        let scope = RepositoryScope::from_repositories(vec![scoped_repo(
            Some(7),
            "demo",
            "/work/demo",
            "/work",
            Some("https://example.com/demo.git"),
        )]);

        assert!(scope.contains(&scoped_repo(
            Some(7),
            "demo",
            "/work/demo",
            "/work",
            Some("https://example.com/demo.git"),
        )));
        assert!(scope.contains(&scoped_repo(
            None,
            "demo",
            "/work/needauth/demo",
            "/work/needauth",
            Some("https://example.com/demo.git"),
        )));
        assert!(!scope.contains(&scoped_repo(
            None,
            "other",
            "/work/other",
            "/work",
            Some("https://example.com/other.git"),
        )));
        assert!(!scope.contains(&scoped_repo(
            None,
            "demo",
            "/other-root/demo",
            "/other-root",
            Some("https://example.com/demo.git"),
        )));
    }

    #[test]
    fn parse_batch_risk_selection_accepts_all() {
        assert_eq!(
            parse_batch_risk_selection("0", 5).unwrap(),
            BatchRiskSelection::All
        );
        assert_eq!(
            parse_batch_risk_selection("1,0,3", 5).unwrap(),
            BatchRiskSelection::All
        );
    }

    #[test]
    fn parse_batch_risk_selection_accepts_number_lists() {
        let selection = parse_batch_risk_selection("1, 3，5", 5).unwrap();
        let BatchRiskSelection::Some(selected) = selection else {
            panic!("应解析为部分选择");
        };

        assert!(selected.contains(&1));
        assert!(selected.contains(&3));
        assert!(selected.contains(&5));
        assert!(!selected.contains(&2));
    }

    #[test]
    fn parse_batch_risk_selection_treats_empty_as_skip_all() {
        assert_eq!(
            parse_batch_risk_selection("   ", 5).unwrap(),
            BatchRiskSelection::None
        );
    }

    #[test]
    fn parse_batch_risk_selection_rejects_invalid_tokens() {
        assert!(parse_batch_risk_selection("abc", 5).is_err());
        assert!(parse_batch_risk_selection("6", 5).is_err());
    }

    #[test]
    fn silent_mode_does_not_bypass_pull_confirmation() {
        let result = validate_pull_confirmation_mode(true, true);
        assert!(result.is_err());
        assert!(validate_pull_confirmation_mode(false, true).is_ok());
    }

    #[test]
    fn partial_fetch_blocks_following_repository_mutation() {
        let steps = vec![WorkflowStep::PullBackup {
            jobs: Some(5),
            diff_after: false,
        }];

        assert!(workflow_steps_modify_repositories(&steps));
        assert!(!workflow_steps_modify_repositories(&[WorkflowStep::Scan {
            output: crate::cli::OutputFormat::Terminal,
            open: false,
            only_dirty_or_behind: false,
        }]));
    }

    #[test]
    fn cli_jobs_and_timeout_override_step_defaults() {
        let workflow = crate::workflow::BuiltInWorkflows::get("daily").unwrap();
        let executor = WorkflowExecutor::new(workflow.clone(), Some(42), Some(90), true, false);
        assert_eq!(executor.resolve_jobs(Some(5)), 42);
        assert_eq!(executor.resolve_timeout(Some(30)), 90);

        let default_executor = WorkflowExecutor::new(workflow, None, None, true, false);
        assert_eq!(default_executor.resolve_jobs(Some(8)), 8);
        assert_eq!(default_executor.resolve_timeout(Some(60)), 60);
    }

    #[test]
    fn workflow_defaults_to_strict_high_risk_policy() {
        let workflow = crate::workflow::BuiltInWorkflows::get("pull-backup").unwrap();
        let strict = WorkflowExecutor::new(workflow, None, None, true, false);
        assert!(!strict.auto_allow_high_risk);
        assert!(!strict.auto_skip_high_risk);
    }

    #[test]
    fn workflow_high_risk_automatic_policies_are_mutually_exclusive() {
        let workflow = crate::workflow::BuiltInWorkflows::get("pull-backup").unwrap();
        let allow = WorkflowExecutor::new(workflow, None, None, true, false)
            .with_auto_allow_high_risk(true);
        assert!(allow.auto_allow_high_risk);
        assert!(!allow.auto_skip_high_risk);

        let skip = allow.with_auto_skip_high_risk(true);
        assert!(skip.auto_skip_high_risk);
        assert!(!skip.auto_allow_high_risk);
    }
}
