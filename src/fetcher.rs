use anyhow::{Context, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::time::timeout;

use crate::db::Database;
use crate::git::{FetchStatus, GitOps, ProxyConfig};
use crate::models::{FetchResult as FetchResultModel, Repository};
use crate::security::{SecurityScanner, format_security_report};
use colored::Colorize;

/// fetch 阶段高风险仓库的批量选择结果。
///
/// fetch 前安全预扫描也可能一次命中大量仓库。这里和 workflow 的 Pull 前确认保持
/// 同样语义：`0` 表示全部继续，空输入表示全部跳过，序号列表表示只继续指定项。
#[derive(Debug, Clone, PartialEq, Eq)]
enum FetchRiskSelection {
    All,
    Some(HashSet<usize>),
    None,
}

/// 解析 fetch 高风险批量确认输入。
///
/// `total` 是当前展示的高风险仓库总数，合法序号从 1 开始。输入非法时返回错误，
/// 调用方会重新提示，避免把手误当成继续执行。
fn parse_fetch_risk_selection(input: &str, total: usize) -> Result<FetchRiskSelection> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(FetchRiskSelection::None);
    }

    let tokens: Vec<_> = trimmed
        .split(|c: char| c == ',' || c == '，' || c.is_whitespace())
        .filter(|token| !token.is_empty())
        .collect();

    if tokens.contains(&"0") {
        return Ok(FetchRiskSelection::All);
    }

    let mut selected = HashSet::new();
    for token in tokens {
        let index = token
            .parse::<usize>()
            .map_err(|_| anyhow::anyhow!("无法识别的序号: {}", token))?;
        if index == 0 || index > total {
            anyhow::bail!("序号 {} 超出范围，应在 1..={} 之间", index, total);
        }
        selected.insert(index);
    }

    Ok(FetchRiskSelection::Some(selected))
}

/// Fetch execution results (includes path change info)
#[derive(Debug, Clone)]
pub struct FetchExecutionResult {
    /// Original repo info (state before fetch)
    pub original_repo: Repository,
    /// Current repository info (after fetch, path updated if moved)
    pub current_repo: Repository,
    /// Whether fetch succeeded
    pub success: bool,
    /// Error message
    pub error: Option<String>,
    /// Execution duration (milliseconds)
    pub duration_ms: u64,
    /// Whether moved to needauth
    pub moved_to_needauth: bool,
    /// Number of retries performed for network errors
    pub retry_count: u32,
    /// Whether restored from needauth (auth issue resolved)
    pub restored_from_needauth: bool,
}

impl FetchExecutionResult {
    /// Get path for database operations (new path after move)
    pub fn db_path(&self) -> &str {
        &self.current_repo.path
    }

    /// Convert to FetchResultModel (for reporting)
    pub fn to_model(&self) -> FetchResultModel {
        FetchResultModel {
            repo_path: self.current_repo.path.clone(),
            success: self.success,
            error: self.error.clone(),
            duration_ms: self.duration_ms,
            retry_count: self.retry_count,
        }
    }
}

/// Concurrent fetch manager
pub struct Fetcher {
    concurrency: usize,
    timeout_secs: u64,
    security_scan: bool,
    auto_skip_high_risk: bool,
    proxy: ProxyConfig,
    move_to_needauth: bool,
    auto_sync: bool,
}

impl Fetcher {
    pub fn new(concurrency: usize, timeout_secs: u64) -> Self {
        Self {
            concurrency,
            timeout_secs,
            security_scan: true,
            auto_skip_high_risk: false,
            proxy: ProxyConfig::default(),
            move_to_needauth: true,
            auto_sync: true,
        }
    }

    pub fn with_security_scan(mut self, enable: bool) -> Self {
        self.security_scan = enable;
        self
    }

    pub fn with_auto_skip_high_risk(mut self, enable: bool) -> Self {
        self.auto_skip_high_risk = enable;
        self
    }

    pub fn with_proxy(mut self, proxy: ProxyConfig) -> Self {
        self.proxy = proxy;
        self
    }

    pub fn with_move_to_needauth(mut self, enable: bool) -> Self {
        self.move_to_needauth = enable;
        self
    }

    /// Set whether to auto-sync before fetch (scan new repositories)
    pub fn with_auto_sync(mut self, enable: bool) -> Self {
        self.auto_sync = enable;
        self
    }

    /// 在 fetch 前后归档远程跟踪分支。
    ///
    /// 归档失败只输出警告，不让整个 fetch 失败：fetch 是用户正在执行的主操作，
    /// 归档是保护层。fetch 前调用用于保护“上一次已经看到的远程引用”，避免本次
    /// fetch 因远程 force-push、删分支或 prune 覆盖旧 tracking ref；fetch 后调用
    /// 用于保护“本次刚看到的新远程引用”。两次调用都经过同一个去重逻辑，latest
    /// 已经指向相同 OID 时不会重复创建历史引用。
    async fn archive_remote_tracking_refs(
        path: PathBuf,
        repo_name: &str,
        timeout_secs: u64,
        phase: &str,
    ) {
        match timeout(
            Duration::from_secs(timeout_secs),
            tokio::task::spawn_blocking(move || GitOps::archive_remote_tracking_refs(&path)),
        )
        .await
        {
            Ok(Ok(Ok(_))) => {}
            Ok(Ok(Err(e))) => eprintln!(
                "   {} {}远程提交归档失败 '{}': {}",
                "⚠️".yellow(),
                phase,
                repo_name,
                e
            ),
            Ok(Err(_)) => eprintln!(
                "   {} {}远程提交归档任务 panic: {}",
                "⚠️".yellow(),
                phase,
                repo_name
            ),
            Err(_) => eprintln!(
                "   {} {}远程提交归档超时: {}",
                "⚠️".yellow(),
                phase,
                repo_name
            ),
        }
    }

    /// Concurrent security scan for all repositories (no stdin interaction)
    ///
    /// 返回高风险仓库路径到安全报告的映射。这里不直接读取 stdin，避免并发任务争抢
    /// 终端输入；真正的交互确认在 `confirm_risky_repos` 中统一串行完成。
    async fn prescan_security_batch(&self, repos: &[Repository]) -> HashMap<String, String> {
        if !self.security_scan {
            return HashMap::new();
        }

        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let mut futures = FuturesUnordered::new();
        let mut risky_reports = HashMap::new();

        for repo in repos {
            let path_str = repo.path.clone();
            let path = std::path::PathBuf::from(&path_str);
            let repo_name = repo.name.clone();
            let timeout_secs = self.timeout_secs;
            let permit = Arc::clone(&semaphore);

            if !path.exists() {
                continue;
            }

            let future = tokio::spawn(async move {
                // SAFETY: semaphore is owned by this function and never closed
                let _permit = permit.acquire().await.expect("semaphore");
                let repo_name_for_err = repo_name.clone();
                let scan_result = match timeout(
                    Duration::from_secs(timeout_secs),
                    tokio::task::spawn_blocking(move || Self::scan_repository(&path, &repo_name)),
                )
                .await
                {
                    Ok(Ok(Ok(r))) => r,
                    // 扫描失败（超时/错误/panic）时按高风险处理，保持 fail-closed。
                    _ => {
                        eprintln!(
                            "  ⚠️ 仓库 '{}' 安全扫描失败，已按高风险处理",
                            repo_name_for_err
                        );
                        return Some((path_str, "安全扫描失败，已按高风险处理".to_string()));
                    }
                };

                if !scan_result.0 {
                    Some((path_str, scan_result.1))
                } else {
                    None
                }
            });
            futures.push(future);
        }

        while let Some(result) = futures.next().await {
            if let Ok(Some((path, report))) = result {
                risky_reports.insert(path, report);
            }
        }

        risky_reports
    }

    /// Confirm high-risk repositories with the user (single-threaded stdin access)
    ///
    /// 高风险仓库先按原始仓库顺序汇总展示，再通过一次输入决定要继续的序号。返回值
    /// 是应跳过的仓库路径集合，后续并发 fetch 会直接略过这些路径。
    fn confirm_risky_repos(
        repos: &[Repository],
        risky_reports: &HashMap<String, String>,
    ) -> HashSet<String> {
        use std::io::{IsTerminal, Write};

        let mut rejected = HashSet::new();

        if risky_reports.is_empty() {
            return rejected;
        }

        if !std::io::stdin().is_terminal() {
            eprintln!("警告：stdin 不是 TTY，默认拒绝所有高风险仓库");
            return risky_reports.keys().cloned().collect();
        }

        let risky_repos: Vec<_> = repos
            .iter()
            .filter(|repo| risky_reports.contains_key(&repo.path))
            .collect();

        eprintln!();
        eprintln!("{}", "🛡️ fetch 前安全扫描命中高风险仓库".yellow().bold());
        eprintln!("{}", "═".repeat(50).yellow());
        eprintln!(
            "输入 {} 表示全部继续；输入序号（如 {}）只继续指定仓库；直接回车表示全部跳过。",
            "0".green().bold(),
            "1,3,5".green()
        );
        eprintln!("继续的仓库会执行本次 fetch；未选择的仓库会保持当前本地状态。");

        for (index, repo) in risky_repos.iter().enumerate() {
            eprintln!();
            eprintln!("[{}] {}", index + 1, repo.name.as_str().bold());
            if let Some(report) = risky_reports.get(&repo.path) {
                eprintln!("{report}");
            }
        }

        let selection = loop {
            eprint!("\n请选择要继续 fetch 的高风险仓库序号（0=全部，回车=全部跳过）: ");
            let _ = std::io::stderr().flush();

            let mut input = String::new();
            match std::io::stdin().read_line(&mut input) {
                Ok(_) => match parse_fetch_risk_selection(&input, risky_repos.len()) {
                    Ok(selection) => break selection,
                    Err(e) => eprintln!("  ⚠️  {}，请重新输入。", e),
                },
                Err(e) => {
                    eprintln!("  ⚠️  读取输入失败: {}，默认全部跳过。", e);
                    break FetchRiskSelection::None;
                }
            }
        };

        for (index, repo) in risky_repos.iter().enumerate() {
            let number = index + 1;
            let continue_fetch = match &selection {
                FetchRiskSelection::All => true,
                FetchRiskSelection::Some(selected) => selected.contains(&number),
                FetchRiskSelection::None => false,
            };

            if continue_fetch {
                eprintln!("  ⚠️  将继续 fetch 高风险仓库 [{}]: {}", number, repo.name);
            } else {
                eprintln!("  ⚠️  已跳过高风险仓库 [{}]: {}", number, repo.name);
                rejected.insert(repo.path.clone());
            }
        }

        rejected
    }

    /// Batch fetch all repositories (with security scan and needauth handling)
    ///
    /// Return detailed execution result for each repository, including path change info.
    /// Does not directly operate on database — caller is responsible for persistence.
    ///
    /// Security scan is performed in two phases to avoid stdin race conditions:
    /// 1. Concurrent scan: check all repos for security risks (no stdin)
    /// 2. Sequential confirm: prompt user for each risky repo (single-threaded stdin)
    pub async fn fetch_all_detailed(
        &self,
        repos: &[Repository],
        progress: bool,
    ) -> Vec<FetchExecutionResult> {
        // Phase 1: Concurrent security scan (no stdin interaction)
        let rejected_paths = if self.security_scan {
            let risky_reports = self.prescan_security_batch(repos).await;
            if self.auto_skip_high_risk {
                // 自动跳过高风险仓库，无需交互确认
                risky_reports.keys().cloned().collect()
            } else {
                // 交互式批量确认所有高风险仓库
                Self::confirm_risky_repos(repos, &risky_reports)
            }
        } else {
            HashSet::new()
        };

        // Phase 2: Concurrent fetch (skip rejected repos)
        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let main_pb = if progress {
            let pb = ProgressBar::new(repos.len() as u64);
            pb.set_style(
                ProgressStyle::default_bar()
                    .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}")
                    .expect("hardcoded template")
                    .progress_chars("#>-"),
            );
            Some(pb)
        } else {
            None
        };

        let mut futures = FuturesUnordered::new();

        for repo in repos {
            // Skip shutdown and rejected repos
            if crate::signal_handler::is_shutdown_requested() {
                break;
            }
            if rejected_paths.contains(&repo.path) {
                if let Some(ref pb) = main_pb {
                    pb.inc(1);
                }
                continue;
            }
            if crate::signal_handler::is_shutdown_requested() {
                break;
            }

            let permit = Arc::clone(&semaphore);
            let repo = repo.clone();
            let timeout_secs = self.timeout_secs;
            let main_pb = main_pb.clone();
            let proxy = self.proxy.clone();
            let move_to_needauth = self.move_to_needauth;

            let future = tokio::spawn(async move {
                // SAFETY: semaphore is owned by this function and never closed
                let _permit = permit.acquire().await.expect("semaphore");
                let start = Instant::now();
                let original_repo = repo.clone();
                let original_path = repo.path.clone();
                let repo_name = repo.name.clone();

                if let Some(ref pb) = main_pb {
                    pb.set_message(repo_name.clone());
                }

                // Check if path exists before fetch
                if !std::path::Path::new(&original_path).exists() {
                    return (
                        original_repo,
                        repo,
                        FetchResultModel {
                            repo_path: original_path.clone(),
                            success: false,
                            error: Some(format!("仓库路径不存在: {}", original_path)),
                            duration_ms: start.elapsed().as_millis() as u64,
                            retry_count: 0,
                        },
                        None,
                        false,
                    );
                }

                // Execute fetch with exponential backoff retry for NetworkError
                let path = std::path::PathBuf::from(&original_path);
                let repo_path = original_path.clone();
                let repo_name = repo.name.clone();
                let root_path = repo.root_path.clone();
                let proxy_for_retry = proxy.clone();

                // fetch 会更新 refs/remotes/*。先归档当前 tracking refs，才能保护
                // “之前已经 fetch 到但这次可能被远程覆盖/删除”的提交。
                Self::archive_remote_tracking_refs(
                    path.clone(),
                    &repo_name,
                    timeout_secs,
                    "fetch 前",
                )
                .await;

                const MAX_RETRIES: u32 = 3;
                let mut retry_count = 0u32;
                let mut fetch_status = FetchStatus::Success;

                // Overall deadline for all attempts combined (prevents unbounded retry time)
                let overall_deadline = tokio::time::Instant::now()
                    + Duration::from_secs(timeout_secs.saturating_mul(2));

                for attempt in 0..=MAX_RETRIES {
                    let path = path.clone();
                    let proxy = proxy_for_retry.clone();

                    let remaining =
                        overall_deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        fetch_status = FetchStatus::NetworkError {
                            message: format!(
                                "整体重试超时 (>{} 秒)",
                                timeout_secs.saturating_mul(2)
                            ),
                        };
                        break;
                    }

                    let attempt_timeout =
                        std::cmp::min(Duration::from_secs(timeout_secs), remaining);

                    fetch_status = match timeout(
                        attempt_timeout,
                        tokio::task::spawn_blocking(move || {
                            let git_ops = GitOps::with_proxy(proxy);
                            git_ops.fetch_detailed(&path, attempt_timeout.as_secs())
                        }),
                    )
                    .await
                    {
                        Ok(Ok((status, _))) => status,
                        Ok(Err(_)) => FetchStatus::OtherError {
                            message: "任务已取消".to_string(),
                        },
                        Err(_) => FetchStatus::NetworkError {
                            message: format!("超时 ({} 秒)", attempt_timeout.as_secs()),
                        },
                    };

                    match &fetch_status {
                        FetchStatus::NetworkError { .. } if attempt < MAX_RETRIES => {
                            let delay_secs = 2u64.pow(attempt);
                            let delay = std::cmp::min(
                                Duration::from_secs(delay_secs),
                                remaining.saturating_sub(Duration::from_millis(100)),
                            );
                            // Ensure minimum 100ms delay to avoid zero-backoff spin
                            let delay = std::cmp::max(delay, Duration::from_millis(100));
                            if remaining < Duration::from_millis(200) {
                                break; // Not enough time remaining for a meaningful retry
                            }
                            tokio::time::sleep(delay).await;
                            retry_count = attempt + 1;
                        }
                        _ => break,
                    }
                }
                if matches!(fetch_status, FetchStatus::Success) {
                    // fetch 成功后再次归档，保护本次新看到的远程 tracking refs。
                    Self::archive_remote_tracking_refs(
                        path.clone(),
                        &repo_name,
                        timeout_secs,
                        "fetch 后",
                    )
                    .await;
                }
                // 3. Handle needauth move or restore
                let (current_repo, result, moved_repo_name, restored_from_needauth) =
                    if fetch_status.should_move_to_needauth() && move_to_needauth {
                        let needauth_dir =
                            std::path::PathBuf::from(&root_path).join(crate::utils::NEEDAUTH_DIR);
                        let needauth_path = needauth_dir.join(&repo_name);

                        let repo_path_clone = repo_path.clone();
                        let upstream_url_clone = repo.upstream_url.clone();
                        let needauth_path_clone = needauth_path.clone();

                        let needauth_dir_clone = needauth_dir.clone();
                        let move_result = match timeout(
                            Duration::from_secs(timeout_secs),
                            tokio::task::spawn_blocking(move || {
                                Self::move_repo_to_needauth(
                                    &repo_path_clone,
                                    &needauth_path_clone,
                                    &needauth_dir_clone,
                                    upstream_url_clone.as_deref(),
                                )
                            }),
                        )
                        .await
                        {
                            Ok(Ok(Ok(path))) => Ok(path),
                            Ok(Ok(Err(e))) => Err(e),
                            Ok(Err(_)) => Err(anyhow::anyhow!("移动任务 panic")),
                            Err(_) => Err(anyhow::anyhow!("移动操作超时 ({} 秒)", timeout_secs)),
                        };

                        match move_result {
                            Ok(final_path) => {
                                let new_repo_path = final_path.to_string_lossy().to_string();
                                let new_root_path = needauth_dir.to_string_lossy().to_string();
                                let final_name = final_path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| repo_name.clone());

                                // Write sidecar file preserving original relative path for accurate restore
                                let original_relative = std::path::Path::new(&original_path)
                                    .strip_prefix(&root_path)
                                    .unwrap_or(std::path::Path::new(&repo_name));
                                let sidecar_path = final_path.join(".needauth_original_path");
                                let _ = std::fs::write(
                                    &sidecar_path,
                                    original_relative.to_string_lossy().as_bytes(),
                                );

                                let mut new_repo = repo.with_new_path(new_repo_path, new_root_path);
                                let name_for_result = final_name.clone();
                                new_repo.name = final_name;

                                let result = FetchResultModel {
                                    repo_path: original_path,
                                    success: false,
                                    error: fetch_status.error_message(),
                                    duration_ms: start.elapsed().as_millis() as u64,
                                    retry_count,
                                };
                                (new_repo, result, Some(name_for_result), false)
                            }
                            Err(e) => {
                                let result = FetchResultModel {
                                    repo_path: original_path,
                                    success: false,
                                    error: Some(format!(
                                        "{}（移动失败: {}）",
                                        fetch_status.error_message().unwrap_or_default(),
                                        e
                                    )),
                                    duration_ms: start.elapsed().as_millis() as u64,
                                    retry_count,
                                };
                                (repo, result, None, false)
                            }
                        }
                    } else if matches!(fetch_status, FetchStatus::Success)
                        && original_path.contains(crate::utils::NEEDAUTH_DIR)
                    {
                        // Restore path: prefer sidecar file (.needauth_original_path) for accurate
                        // nested path restoration; fall back to direct-child assumption for legacy entries.
                        let sidecar_path =
                            std::path::Path::new(&original_path).join(".needauth_original_path");
                        let original_repo_path =
                            if let Ok(relative) = std::fs::read_to_string(&sidecar_path) {
                                let relative = relative.trim();
                                std::path::PathBuf::from(&root_path).join(relative)
                            } else {
                                let needauth_parent = std::path::Path::new(&original_path)
                                    .parent()
                                    .and_then(|p| p.parent())
                                    .map(|p| p.to_path_buf())
                                    .unwrap_or_else(|| std::path::PathBuf::from(&root_path));
                                needauth_parent.join(&repo_name)
                            };
                        // needauth_parent 必须是扫描根目录，而非嵌套父目录，否则 root_path 会被错误记录
                        let needauth_parent = std::path::PathBuf::from(&root_path);

                        let from_path = original_path.clone();
                        let upstream = repo.upstream_url.clone();
                        let needauth_parent_clone = needauth_parent.clone();
                        let restore_result = match timeout(
                            Duration::from_secs(timeout_secs),
                            tokio::task::spawn_blocking(move || {
                                Self::move_repo_from_needauth(
                                    &from_path,
                                    &original_repo_path,
                                    &needauth_parent_clone,
                                    upstream.as_deref(),
                                )
                            }),
                        )
                        .await
                        {
                            Ok(Ok(Ok(path))) => Ok(path),
                            Ok(Ok(Err(e))) => Err(e),
                            Ok(Err(_)) => Err(anyhow::anyhow!("恢复任务 panic")),
                            Err(_) => Err(anyhow::anyhow!("恢复操作超时 ({} 秒)", timeout_secs)),
                        };

                        match restore_result {
                            Ok(restored_path) => {
                                let new_path = restored_path.to_string_lossy().to_string();
                                // root_path 必须是原始扫描根目录，不能是嵌套父目录
                                let new_root = root_path.clone();
                                let mut restored_repo = repo.with_new_path(new_path, new_root);
                                restored_repo.name = repo_name.clone();

                                // Clean up sidecar file after successful restore
                                let _ = std::fs::remove_file(
                                    restored_path.join(".needauth_original_path"),
                                );

                                let result = FetchResultModel {
                                    repo_path: restored_repo.path.clone(),
                                    success: true,
                                    error: None,
                                    duration_ms: start.elapsed().as_millis() as u64,
                                    retry_count,
                                };
                                (restored_repo, result, None, true)
                            }
                            Err(e) => {
                                let result = FetchResultModel {
                                    repo_path: original_path,
                                    success: true,
                                    error: Some(format!(
                                        "Fetch 成功，但从 needauth 恢复失败: {}",
                                        e
                                    )),
                                    duration_ms: start.elapsed().as_millis() as u64,
                                    retry_count,
                                };
                                (repo, result, None, false)
                            }
                        }
                    } else {
                        let result = FetchResultModel {
                            repo_path: original_path,
                            success: matches!(fetch_status, FetchStatus::Success),
                            error: fetch_status.error_message(),
                            duration_ms: start.elapsed().as_millis() as u64,
                            retry_count,
                        };
                        (repo, result, None, false)
                    };

                if let Some(ref pb) = main_pb {
                    pb.inc(1);
                }

                (
                    original_repo,
                    current_repo,
                    result,
                    moved_repo_name,
                    restored_from_needauth,
                )
            });

            futures.push(future);
        }

        let mut results = Vec::new();
        let mut moved_repos: Vec<String> = Vec::new();
        let mut restored_repos: Vec<String> = Vec::new();

        while !futures.is_empty() {
            match timeout(Duration::from_millis(200), futures.next()).await {
                Ok(Some(join_result)) => {
                    match join_result {
                        Ok((original_repo, current_repo, result_model, moved_name, restored)) => {
                            let moved = moved_name.is_some();

                            // 在移动 current_repo 之前收集已恢复仓库的名称
                            if restored {
                                restored_repos.push(current_repo.name.clone());
                            }

                            let exec_result = FetchExecutionResult {
                                original_repo,
                                current_repo,
                                success: result_model.success,
                                error: result_model.error,
                                duration_ms: result_model.duration_ms,
                                moved_to_needauth: moved,
                                retry_count: result_model.retry_count,
                                restored_from_needauth: restored,
                            };

                            // 收集已移动的仓库
                            if let Some(name) = moved_name {
                                moved_repos.push(name);
                            }

                            results.push(exec_result);
                        }
                        Err(e) => {
                            eprintln!("  │   {} 任务 panic: {}", "⚠️".yellow(), e);
                            // Task panic 时不推入空记录，避免下游数据库操作使用空路径导致异常。
                            // 统计信息在 fetch_and_update 中通过 repos.len() - results.len() 补偿。
                            if let Some(ref pb) = main_pb {
                                pb.inc(1);
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    if crate::signal_handler::is_shutdown_requested() {
                        eprintln!("  ⚠️  收到中断信号，正在取消剩余任务");
                        break;
                    }
                }
            }
        }

        // 进度条完成后，统一显示各类信息
        if let Some(pb) = main_pb {
            pb.finish_and_clear();
        }

        if !moved_repos.is_empty() {
            println!("  ├─ {} 以下仓库已移动到 needauth/:", "📁".yellow());
            for (i, name) in moved_repos.iter().enumerate() {
                let is_last = i == moved_repos.len() - 1;
                let corner = if is_last { "└─" } else { "├─" };
                println!("  │   {} {}", corner, name.dimmed());
            }
        }

        if !restored_repos.is_empty() {
            println!("  ├─ {} 以下仓库已从 needauth/ 恢复:", "📁".green());
            for (i, name) in restored_repos.iter().enumerate() {
                let is_last = i == restored_repos.len() - 1;
                let corner = if is_last { "└─" } else { "├─" };
                println!("  │   {} {}", corner, name.green());
            }
        }

        results
    }

    /// 跨文件系统安全移动目录
    ///
    /// 先尝试 `fs::rename`（原子、高效），若因跨文件系统失败（EXDEV），
    /// 则回退到复制后删除源目录。
    fn move_or_copy_dir(from: &std::path::Path, to: &std::path::Path) -> Result<(), anyhow::Error> {
        // 先尝试原子 rename
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                // 跨文件系统，继续回退逻辑
            }
            Err(e) => return Err(e.into()),
        }

        // 回退：复制目录树后删除源
        #[cfg(unix)]
        {
            let status = std::process::Command::new("cp")
                .args(["-a", &from.to_string_lossy(), &to.to_string_lossy()])
                .status()
                .with_context(|| {
                    format!(
                        "无法启动 cp -a 复制: {} -> {}",
                        from.display(),
                        to.display()
                    )
                })?;

            if !status.success() {
                return Err(anyhow::anyhow!(
                    "cp -a 复制失败 (exit code: {:?}): {} -> {}",
                    status.code(),
                    from.display(),
                    to.display()
                ));
            }

            std::fs::remove_dir_all(from)
                .with_context(|| format!("复制成功后无法删除源目录: {}", from.display()))?;
        }

        #[cfg(windows)]
        {
            let status = std::process::Command::new("robocopy")
                .args([
                    &from.to_string_lossy(),
                    &to.to_string_lossy(),
                    "/E",
                    "/MOVE",
                ])
                .status()
                .with_context(|| {
                    format!("无法启动 robocopy: {} -> {}", from.display(), to.display())
                })?;

            // robocopy 退出码 0-7 表示成功
            if status.code().unwrap_or(999) > 7 {
                return Err(anyhow::anyhow!(
                    "robocopy 失败 (exit code: {:?}): {} -> {}",
                    status.code(),
                    from.display(),
                    to.display()
                ));
            }
        }

        Ok(())
    }

    /// Move repository to needauth directory
    ///
    /// Move from normal scan directory to `<root_path>/needauth/<repo_name>/`.
    ///
    /// # Same-name repository handling
    /// If a same-name repository already exists in needauth, compare upstream_url:
    /// - Same: considered same repository, overwrite old one
    /// - Different: different author's repository, rename with numeric suffix (e.g. `repo-2`, `repo-3`)
    ///
    /// # Atomicity guarantee
    /// Use two-phase strategy of "rename target to temp first, then rename source→target",
    /// avoiding data loss from being killed between `remove_dir_all` + `rename`.
    ///
    /// # Path equality protection
    /// Use `canonicalize` to compare source and target paths, preventing repos already in needauth
    /// from emptying themselves (self-reference problem).
    fn move_repo_to_needauth(
        from: &str,
        to: &std::path::Path,
        expected_parent: &std::path::Path,
        upstream_url: Option<&str>,
    ) -> Result<std::path::PathBuf, anyhow::Error> {
        use std::fs;
        use std::path::Path;

        let from_path = Path::new(from);

        // ── Path traversal protection ────────────────────────────────────────────────
        // Ensure `to` path is within expected needauth directory, prevent ../../../etc attacks
        // 1. First verify target path doesn't contain path traversal components
        // 前置路径遍历检查已移除：canonicalize + starts_with 后续会提供完整防护，
        // 且 contains("..") 无法防御 Unicode 编码绕过（如 %2e%2e），还可能误报合法路径（如 foo..bar）。

        // 2. Ensure parent directory exists and is resolvable (create before move)
        let parent = to
            .parent()
            .ok_or_else(|| anyhow::anyhow!("目标路径没有父目录: {}", to.display()))?;

        // 3. Create parent directory (if it doesn't exist)
        fs::create_dir_all(parent)
            .with_context(|| format!("无法创建目标父目录: {}", parent.display()))?;

        // 4. canonicalize parent directory to get absolute path
        let parent_canonical = parent
            .canonicalize()
            .with_context(|| format!("无法解析父目录: {}", parent.display()))?;

        // 5. Build canonicalized target path (using parent canonical path + target file name)
        let file_name = to
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("无法获取目标文件名: {}", to.display()))?;
        let to_canonical = parent_canonical.join(file_name);

        // 6. Strict verification: target path must be a child of parent directory
        // Note: starts_with on Path is safe here because parent directory is canonicalized
        if !to_canonical.starts_with(&parent_canonical) {
            return Err(anyhow::anyhow!(
                "检测到路径遍历风险：目标路径 '{}' 不在预期目录 '{}' 内",
                to_canonical.display(),
                parent_canonical.display()
            ));
        }

        // 7. Verify target path is within the expected parent directory (defense in depth)
        if !expected_parent.exists() {
            fs::create_dir_all(expected_parent)
                .with_context(|| format!("无法创建预期父目录: {}", expected_parent.display()))?;
        }
        let expected_canonical = expected_parent
            .canonicalize()
            .with_context(|| format!("无法解析预期父目录: {}", expected_parent.display()))?;
        if !to_canonical.starts_with(&expected_canonical) {
            return Err(anyhow::anyhow!(
                "检测到路径遍历风险：目标路径 '{}' 不在预期目录 '{}' 内",
                to_canonical.display(),
                expected_canonical.display()
            ));
        }

        // ── Path equality protection ──────────────────────────────────────────────
        // When repository is already in needauth, `root_path` is still the original scan root,
        // the calculated needauth_path happens to equal from.
        // Note: from_path may not exist (already moved or deleted), so use try_canonicalize
        match Self::try_canonicalize(from_path) {
            Some(from_canon) if from_canon == to_canonical => {
                return Ok(to.to_path_buf());
            }
            _ => {
                // from doesn't exist or differs from to, continue move process
            }
        }

        // ── Determine final target path ────────────────────────────────────────────
        // If target exists, check if it's the same repository
        let final_to = if to.exists() {
            // Try to read target repository's remote URL
            let target_url = Self::get_repo_remote_url(to);

            if target_url.is_some()
                && upstream_url.is_some()
                && target_url.as_deref() != upstream_url
            {
                // Same name but different author, needs renaming
                let renamed = Self::find_unique_repo_name(to)?;
                eprintln!(
                    "   ⚠️ needauth 中已存在同名但来源不同的仓库，已重命名为: {}",
                    renamed.file_name().unwrap_or_default().to_string_lossy()
                );
                renamed
            } else {
                // Same repository or unable to compare, overwrite
                to.to_path_buf()
            }
        } else {
            to.to_path_buf()
        };

        // ── Ensure parent directory exists ──────────────────────────────────────────────
        // Note: if final_to is renamed to repo-2 etc., may need to create new parent directory
        if let Some(parent) = final_to.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("无法创建最终父目录: {}", parent.display()))?;
        }

        // ── Two-phase atomic move ──────────────────────────────────────────────
        // Phase 1: if target exists, move to temp directory first
        // Phase 2: move source to target
        // Phase 3: clean up temp directory
        //
        // Note: if process crashes after phase 2, temp files will remain.
        // Files ending with .getlatestrepo_swap can be deleted via periodic cleanup scripts.
        let tmp_path;
        if final_to.exists() {
            tmp_path = Self::unique_temp_path(&final_to);
            if let Err(e) = fs::rename(&final_to, &tmp_path) {
                return Err(anyhow::anyhow!(
                    "无法将现有仓库移动到临时位置 '{}': {}",
                    tmp_path.display(),
                    e
                ));
            }
        } else {
            tmp_path = PathBuf::new();
        }

        // Execute move
        if let Err(e) = Self::move_or_copy_dir(Path::new(from), &final_to) {
            // If move fails, try to restore original target from temp
            if !tmp_path.as_os_str().is_empty()
                && let Err(restore_err) = Self::move_or_copy_dir(&tmp_path, &final_to)
            {
                return Err(anyhow::anyhow!(
                    "严重错误：无法将仓库移动到 '{}': {}。此外，从临时位置 '{}' 恢复原始目标也失败: {}。原始数据可能位于 '{}'。",
                    final_to.display(),
                    e,
                    tmp_path.display(),
                    restore_err,
                    tmp_path.display()
                ));
            }
            return Err(anyhow::anyhow!(
                "无法将仓库移动到 '{}': {}",
                final_to.display(),
                e
            ));
        }

        // Clean up temp directory (failure is not an error, as this is best-effort cleanup)
        if !tmp_path.as_os_str().is_empty()
            && let Err(e) = fs::remove_dir_all(&tmp_path)
        {
            eprintln!(
                "警告：无法清理临时目录 '{}': {}。请手动删除。",
                tmp_path.display(),
                e
            );
        }

        Ok(final_to)
    }

    /// Move repository from needauth directory back to original location
    ///
    /// Used when a previously authentication-failed repository successfully fetches again,
    /// indicating the authentication issue has been resolved.
    ///
    /// # Safety rules
    /// - If target path exists and is the same repository (upstream_url matches) → skip (don't overwrite)
    /// - If target path exists but is a different repository → skip (preserve user's new clone)
    /// - If target path exists but is not a git repository → skip (don't overwrite non-git data)
    /// - If target path does not exist → execute two-phase atomic move
    fn move_repo_from_needauth(
        from: &str,
        to: &std::path::Path,
        expected_parent: &std::path::Path,
        upstream_url: Option<&str>,
    ) -> Result<std::path::PathBuf, anyhow::Error> {
        use std::fs;
        use std::path::Path;

        let from_path = Path::new(from);

        // ── Path traversal protection ────────────────────────────────────────────────
        // 前置路径遍历检查已移除：canonicalize + starts_with 后续会提供完整防护，
        // 且 contains("..") 无法防御 Unicode 编码绕过（如 %2e%2e），还可能误报合法路径（如 foo..bar）。

        let parent = to
            .parent()
            .ok_or_else(|| anyhow::anyhow!("目标路径没有父目录: {}", to.display()))?;

        let parent_canonical = parent
            .canonicalize()
            .with_context(|| format!("无法解析父目录: {}", parent.display()))?;

        let file_name = to
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("无法获取目标文件名: {}", to.display()))?;
        let to_canonical = parent_canonical.join(file_name);

        if !to_canonical.starts_with(&parent_canonical) {
            return Err(anyhow::anyhow!(
                "检测到路径遍历风险：目标路径 '{}' 不在预期目录 '{}' 内",
                to_canonical.display(),
                parent_canonical.display()
            ));
        }

        // Verify target path is within the expected parent directory (defense in depth)
        if !expected_parent.exists() {
            fs::create_dir_all(expected_parent)
                .with_context(|| format!("无法创建预期父目录: {}", expected_parent.display()))?;
        }
        let expected_canonical = expected_parent
            .canonicalize()
            .with_context(|| format!("无法解析预期父目录: {}", expected_parent.display()))?;
        if !to_canonical.starts_with(&expected_canonical) {
            return Err(anyhow::anyhow!(
                "检测到路径遍历风险：目标路径 '{}' 不在预期目录 '{}' 内",
                to_canonical.display(),
                expected_canonical.display()
            ));
        }

        // ── Self-reference protection ──────────────────────────────────────────────
        match Self::try_canonicalize(from_path) {
            Some(from_canon) if from_canon == to_canonical => {
                return Ok(to.to_path_buf());
            }
            _ => {}
        }

        // ── Target existence check ──────────────────────────────────────────────
        if to.exists() {
            let target_url = Self::get_repo_remote_url(to);
            if target_url.is_some()
                && upstream_url.is_some()
                && target_url.as_deref() == upstream_url
            {
                // Same repository already exists at target — user likely re-cloned it
                // Clean up the needauth copy and return Ok so the caller updates the DB path
                if from_path.exists()
                    && let Err(e) = std::fs::remove_dir_all(from_path)
                {
                    return Err(anyhow::anyhow!(
                        "检测到目标位置已有重复仓库后，无法清理 needauth 副本 '{}': {}",
                        from_path.display(),
                        e
                    ));
                }
                return Ok(to.to_path_buf());
            } else if to.join(".git").exists() {
                // Different repository exists at target — don't overwrite
                return Err(anyhow::anyhow!(
                    "目标路径 '{}' 已包含另一个仓库，跳过恢复",
                    to.display()
                ));
            } else {
                // Non-git directory exists at target — don't overwrite
                return Err(anyhow::anyhow!(
                    "目标路径 '{}' 已存在且不是 Git 仓库，跳过恢复",
                    to.display()
                ));
            }
        }

        // ── Ensure parent directory exists ──────────────────────────────────────────────
        fs::create_dir_all(parent)
            .with_context(|| format!("无法创建父目录: {}", parent.display()))?;

        // ── Two-phase atomic move ──────────────────────────────────────────────
        if let Err(e) = Self::move_or_copy_dir(Path::new(from), &to_canonical) {
            return Err(anyhow::anyhow!(
                "无法将仓库从 '{}' 移动到 '{}': {}",
                from,
                to_canonical.display(),
                e
            ));
        }

        Ok(to_canonical)
    }

    /// Try to canonicalize path, return None if it doesn't exist
    fn try_canonicalize(path: &std::path::Path) -> Option<std::path::PathBuf> {
        path.canonicalize().ok()
    }

    /// Get repository's remote URL
    fn get_repo_remote_url(path: &std::path::Path) -> Option<String> {
        let repo = git2::Repository::open(path).ok()?;
        repo.find_remote("origin")
            .ok()
            .and_then(|r| r.url().map(|u| u.to_string()))
    }

    /// Generate unique name for same-name different-author repositories
    /// Format: repo-name-2, repo-name-3, ...
    fn find_unique_repo_name(base: &std::path::Path) -> Result<std::path::PathBuf, anyhow::Error> {
        let parent = base
            .parent()
            .ok_or_else(|| anyhow::anyhow!("无法获取父目录"))?;
        let name = base
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("无法获取仓库名称"))?
            .to_string_lossy();

        // Try repo-name-2, repo-name-3, ...
        for i in 2u32.. {
            let candidate = parent.join(format!("{}-{}", name, i));
            if !candidate.exists() {
                return Ok(candidate);
            }
        }

        // Theoretically unreachable
        Err(anyhow::anyhow!("无法找到唯一的仓库名称"))
    }

    /// Generate a non-conflicting temp path
    fn unique_temp_path(target: &std::path::Path) -> std::path::PathBuf {
        let base = target.with_extension("getlatestrepo_swap");
        if !base.exists() {
            return base;
        }
        for i in 1u32.. {
            let mut candidate = base.as_os_str().to_os_string();
            candidate.push(format!(".{}", i));
            let candidate = std::path::PathBuf::from(candidate);
            if !candidate.exists() {
                return candidate;
            }
        }
        // Theoretically unreachable
        let mut fallback = base.as_os_str().to_os_string();
        fallback.push(format!(".{}", std::process::id()));
        std::path::PathBuf::from(fallback)
    }

    /// Scan single repository's security
    fn scan_repository(
        path: &std::path::Path,
        _name: &str,
    ) -> Result<(bool, String), anyhow::Error> {
        let repo = git2::Repository::open(path)?;
        let local_oid = repo.head().ok().and_then(|h| h.target());
        let remote_oid = Self::get_remote_oid(&repo)?;

        if local_oid.is_none() || remote_oid.is_none() {
            return Ok((true, String::new()));
        }

        let result = SecurityScanner::scan_before_fetch(path, local_oid, remote_oid)?;
        let report = format_security_report(&result);
        Ok((result.is_safe, report))
    }

    /// Get remote branch OID
    fn get_remote_oid(repo: &git2::Repository) -> Result<Option<git2::Oid>, anyhow::Error> {
        let remote_name = GitOps::get_remote_name(repo)
            .ok()
            .flatten()
            .unwrap_or_else(|| "origin".to_string());
        let branch_names = [
            format!("{}/HEAD", remote_name),
            format!("{}/main", remote_name),
            format!("{}/master", remote_name),
            format!("{}/develop", remote_name),
        ];

        for branch_name in &branch_names {
            if let Ok(reference) = repo.find_reference(&format!("refs/remotes/{}", branch_name)) {
                return Ok(reference.target());
            }
        }

        Ok(None)
    }

    /// Fetch and update database
    ///
    /// Correctly handle path updates after repository move:
    /// - If repository moved to needauth, update path, root_path, depth in database
    /// - Only update fetch time for successful fetches
    /// - Optional: auto-sync before fetch (scan new repositories)
    pub async fn fetch_and_update(
        &self,
        repos: &[Repository],
        _db: &Database,
        progress: bool,
    ) -> Result<FetchSummary> {
        let exec_results = self.fetch_all_detailed(repos, progress).await;

        let mut summary = FetchSummary::new();

        for exec_result in &exec_results {
            if exec_result.success {
                summary.success += 1;
            } else {
                summary.failed += 1;
            }
            summary.total += 1;
        }

        // 补偿因 panic 或其他原因未返回结果的仓库数量
        let missing_count = repos.len().saturating_sub(exec_results.len());
        if missing_count > 0 {
            summary.failed += missing_count;
            summary.total += missing_count;
        }

        summary.results = exec_results.iter().map(|r| r.to_model()).collect();

        // Opens its own DB connection because Database is not Send + 'static.
        // DB failures are logged but don't abort the batch — fetch results are still valid.
        let db_result = tokio::task::spawn_blocking(move || -> Result<()> {
            let db = Database::open().map_err(|e| {
                eprintln!("fetch 更新时无法打开数据库: {}", e);
                e
            })?;
            for exec_result in exec_results {
                // 防御性过滤：跳过空路径记录（通常由 task panic 导致）
                if exec_result.current_repo.path.is_empty() {
                    eprintln!("警告：跳过空路径记录（task 可能 panic），避免污染数据库");
                    continue;
                }
                if exec_result.success
                    && let Err(e) = db.update_fetch_time(exec_result.db_path())
                {
                    eprintln!(
                        "更新 fetch 时间失败 '{}': {}",
                        crate::utils::sanitize_path(exec_result.db_path()),
                        e
                    );
                }
                // If the repository was moved to need-auth, atomically update the database
                if exec_result.moved_to_needauth {
                    let mut moved_repo = exec_result.current_repo.clone();
                    if let Err(e) =
                        db.move_repository(&exec_result.original_repo.path, &mut moved_repo)
                    {
                        eprintln!(
                            "移动仓库数据库记录失败 '{}': {}",
                            crate::utils::sanitize_path(exec_result.db_path()),
                            e
                        );
                    }
                }
                // If the repository was restored from needauth (auth resolved), update the database path
                if exec_result.restored_from_needauth {
                    let mut restored_repo = exec_result.current_repo.clone();
                    if let Err(e) =
                        db.move_repository(&exec_result.original_repo.path, &mut restored_repo)
                    {
                        eprintln!(
                            "恢复仓库数据库记录失败 '{}': {}",
                            crate::utils::sanitize_path(exec_result.db_path()),
                            e
                        );
                    }
                }
            }
            Ok(())
        })
        .await;

        // DB update failures are non-fatal — fetch results are already captured in summary
        match db_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("警告：fetch 后数据库更新出现错误: {}", e),
            Err(e) => eprintln!("警告：fetch 后数据库更新任务失败: {}", e),
        }

        Ok(summary)
    }

    /// fetch 后重新扫描状态
    ///
    /// 正确处理仓库移动：
    /// - 若仓库已移动到 needauth，使用新路径重新扫描
    /// - 保留 fetch 时间等元数据
    pub async fn fetch_and_rescan(
        &self,
        repos: &[Repository],
        db: &Database,
        progress: bool,
    ) -> Result<Vec<Repository>> {
        // Get detailed execution results (include path change information)
        let exec_results = self.fetch_all_detailed(repos, progress).await;

        let mut updated_repos = Vec::new();

        // 构建从原始路径到执行结果的映射，用于快速查找
        // 防御性过滤：排除空路径记录（通常由 task panic 导致），避免 map 键冲突
        let result_map: std::collections::HashMap<String, &FetchExecutionResult> = exec_results
            .iter()
            .filter(|r| !r.original_repo.path.is_empty())
            .map(|r| (r.original_repo.path.clone(), r))
            .collect();

        for repo in repos {
            if crate::signal_handler::is_shutdown_requested() {
                eprintln!("  ⚠️  收到中断信号，跳过剩余仓库扫描");
                break;
            }

            // 查找对应的执行结果
            let exec_result = result_map.get(&repo.path);

            // 使用当前路径（可能是新路径）重新扫描
            let path_to_scan = exec_result.map(|r| r.db_path()).unwrap_or(&repo.path);
            let root_path = exec_result
                .map(|r| &r.current_repo.root_path)
                .unwrap_or(&repo.root_path);

            // 检查路径是否存在
            if !std::path::Path::new(path_to_scan).exists() {
                eprintln!(
                    "   {} 仓库路径不存在，跳过重新扫描: {}",
                    "⚠️".yellow(),
                    path_to_scan
                );
                // 若路径不存在，从数据库中删除该记录
                if let Err(e) = db.delete_repository(path_to_scan) {
                    eprintln!("   {} 从数据库删除失败: {}", "⚠️".yellow(), e);
                }
                continue;
            }

            let path_buf = std::path::PathBuf::from(path_to_scan);
            let root_path_str = root_path.to_string();
            let inspect_result = match timeout(
                Duration::from_secs(self.timeout_secs),
                tokio::task::spawn_blocking(move || GitOps::inspect(&path_buf, &root_path_str)),
            )
            .await
            {
                Ok(Ok(Ok(r))) => Ok(r),
                Ok(Ok(Err(e))) => Err(e),
                Ok(Err(_)) => Err(crate::error::GetLatestRepoError::Other(anyhow::anyhow!(
                    "Inspect 任务 panic"
                ))),
                Err(_) => Err(crate::error::GetLatestRepoError::Other(anyhow::anyhow!(
                    "Inspect 超时 ({} 秒)",
                    self.timeout_secs
                ))),
            };

            match inspect_result {
                Ok(mut updated) => {
                    // 保留原始元数据
                    updated.id = repo.id;
                    if let Some(exec_result) = exec_result {
                        // 若 fetch 成功，使用原始 fetch 时间；否则保留数据库中的值
                        if exec_result.success {
                            updated.last_fetch_at = Some(chrono::Local::now());
                        } else {
                            updated.last_fetch_at = repo.last_fetch_at;
                        }
                    } else {
                        updated.last_fetch_at = repo.last_fetch_at;
                    }

                    // 若从 needauth 恢复，原子性地移动数据库记录（删除旧记录 + 插入新记录）
                    // 避免在旧 needauth 路径留下孤儿记录
                    let db_result = if exec_result
                        .map(|r| r.restored_from_needauth)
                        .unwrap_or(false)
                    {
                        db.move_repository(&repo.path, &mut updated)
                    } else {
                        db.upsert_repository(&mut updated)
                    };
                    if let Err(e) = db_result {
                        eprintln!("更新仓库失败 '{}': {}", updated.name, e);
                    }
                    updated_repos.push(updated);
                }
                Err(e) => {
                    // 若扫描失败（例如仓库移动到了无效路径），记录错误但保留原始信息
                    eprintln!("重新扫描失败 '{}': {}", repo.name, e);

                    // If the repository was moved or restored, try to use the current info
                    if let Some(exec_result) = exec_result {
                        if exec_result.moved_to_needauth {
                            // Update the DB record to point to the needauth path, even though rescan failed
                            let mut moved_repo = exec_result.current_repo.clone();
                            moved_repo.last_fetch_at = repo.last_fetch_at;
                            if let Err(e) =
                                db.move_repository(&exec_result.original_repo.path, &mut moved_repo)
                            {
                                eprintln!(
                                    "错误：数据库记录移动到 needauth 失败 '{}': {}",
                                    moved_repo.name, e
                                );
                                // Attempt filesystem rollback: move repo back to original path
                                let needauth_path = std::path::PathBuf::from(&moved_repo.path);
                                let original_path =
                                    std::path::PathBuf::from(&exec_result.original_repo.path);
                                if needauth_path.exists() && !original_path.exists() {
                                    if let Err(rollback_err) =
                                        std::fs::rename(&needauth_path, &original_path)
                                    {
                                        eprintln!(
                                            "严重错误：文件系统回滚也失败 '{}': {}",
                                            moved_repo.name, rollback_err
                                        );
                                        eprintln!(
                                            "  仓库位于 '{}'，但数据库仍指向 '{}'",
                                            needauth_path.display(),
                                            original_path.display()
                                        );
                                    } else {
                                        eprintln!(
                                            "  文件系统回滚成功：仓库已恢复到 '{}'",
                                            original_path.display()
                                        );
                                        moved_repo.path = exec_result.original_repo.path.clone();
                                    }
                                }
                            }
                            updated_repos.push(moved_repo);
                            continue;
                        } else if exec_result.restored_from_needauth {
                            // Use the restored repository info and atomically update the DB record
                            let mut restored_repo = exec_result.current_repo.clone();
                            restored_repo.last_fetch_at = repo.last_fetch_at;
                            if let Err(e) = db.move_repository(
                                &exec_result.original_repo.path,
                                &mut restored_repo,
                            ) {
                                eprintln!(
                                    "恢复仓库数据库记录失败（重新扫描错误）'{}': {}",
                                    restored_repo.name, e
                                );
                            }
                            updated_repos.push(restored_repo);
                            continue;
                        }
                    }

                    updated_repos.push(repo.clone());
                }
            }
        }

        Ok(updated_repos)
    }
}

/// Fetch result summary
#[derive(Debug)]
pub struct FetchSummary {
    pub total: usize,
    pub success: usize,
    pub failed: usize,
    pub results: Vec<FetchResultModel>,
}

impl FetchSummary {
    pub fn new() -> Self {
        Self {
            total: 0,
            success: 0,
            failed: 0,
            results: Vec::new(),
        }
    }

    pub fn print_summary(&self) {
        println!("\n📊 Fetch 结果:");

        // 按错误类型分类
        let mut network_failures = Vec::new();
        let mut auth_failures = Vec::new();
        let mut other_failures = Vec::new();

        for result in &self.results {
            if !result.success
                && let Some(ref error) = result.error
            {
                if error.contains("网络错误")
                    || error.contains("超时")
                    || error.contains("Rate limited")
                    || error.contains("rate limited")
                    || error.contains("Timeout")
                {
                    network_failures.push(result);
                } else if error.contains("需要认证")
                    || error.contains("仓库不存在")
                    || error.contains("移动失败")
                    || error.contains("移动任务 panic")
                    || error.contains("Authentication required")
                    || error.contains("Repository not found")
                {
                    auth_failures.push(result);
                } else {
                    other_failures.push(result);
                }
            }
        }

        if self.failed > 0 {
            println!(
                "   总计: {} | 成功: {} | 失败: {}（网络: {}，认证/仓库: {}，其他: {}）",
                self.total,
                self.success,
                self.failed,
                network_failures.len(),
                auth_failures.len(),
                other_failures.len()
            );
        } else {
            println!(
                "   总计: {} | 成功: {} | 失败: {}",
                self.total, self.success, self.failed
            );
        }

        if self.failed > 0 {
            println!("\n⚠️ 失败详情:");

            let print_group = |label: &str, icon: &str, items: &[&FetchResultModel]| {
                if items.is_empty() {
                    return;
                }
                println!("   {icon} {label} ({})", items.len());
                for (i, result) in items.iter().enumerate() {
                    let is_last = i == items.len() - 1;
                    let corner = if is_last { "└─" } else { "├─" };
                    let short_path = std::path::Path::new(&result.repo_path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&result.repo_path);
                    let retry_info = if result.retry_count > 0 {
                        format!("（已重试 {} 次）", result.retry_count)
                    } else {
                        String::new()
                    };
                    println!(
                        "      {corner} {short_path}{retry_info}: {}",
                        result.error.as_deref().unwrap_or("未知错误")
                    );
                }
            };

            print_group("网络错误", "🔌", &network_failures);
            print_group("认证/仓库错误", "🔒", &auth_failures);
            print_group("其他错误", "❌", &other_failures);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    #[test]
    fn parse_fetch_risk_selection_accepts_all() {
        assert_eq!(
            parse_fetch_risk_selection("0", 5).unwrap(),
            FetchRiskSelection::All
        );
        assert_eq!(
            parse_fetch_risk_selection("2,0,4", 5).unwrap(),
            FetchRiskSelection::All
        );
    }

    #[test]
    fn parse_fetch_risk_selection_accepts_number_lists() {
        let selection = parse_fetch_risk_selection("1 3，5", 5).unwrap();
        let FetchRiskSelection::Some(selected) = selection else {
            panic!("应解析为部分选择");
        };

        assert!(selected.contains(&1));
        assert!(selected.contains(&3));
        assert!(selected.contains(&5));
        assert!(!selected.contains(&2));
    }

    #[test]
    fn parse_fetch_risk_selection_treats_empty_as_skip_all() {
        assert_eq!(
            parse_fetch_risk_selection("   ", 5).unwrap(),
            FetchRiskSelection::None
        );
    }

    #[test]
    fn parse_fetch_risk_selection_rejects_invalid_tokens() {
        assert!(parse_fetch_risk_selection("abc", 5).is_err());
        assert!(parse_fetch_risk_selection("6", 5).is_err());
    }

    /// 运行测试用 git 命令。
    ///
    /// fetch 归档保护依赖原生 `git fetch` 更新 `refs/remotes/*`。这里用本地 bare
    /// remote 和普通工作仓库搭出真实 fetch 流程，比 mock 更能覆盖旧 tracking ref
    /// 在 fetch 前被归档、新 tracking ref 在 fetch 后被归档的行为。
    fn run_git(args: &[&str], cwd: Option<&std::path::Path>) -> String {
        let mut command = Command::new("git");
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = command.output().expect("git command should start");
        assert!(
            output.status.success(),
            "git {:?} failed\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git stdout should be utf-8")
            .trim()
            .to_string()
    }

    fn list_remote_archive_oids(path: &std::path::Path) -> Vec<git2::Oid> {
        let repo = git2::Repository::open(path).expect("open repo");
        let mut oids = Vec::new();
        let refs = repo
            .references_glob("refs/glr-remote-archive/*")
            .expect("list archive refs");
        for reference in refs.flatten() {
            if let Some(oid) = reference.target() {
                oids.push(oid);
            }
        }
        oids
    }

    #[tokio::test]
    async fn fetch_archives_remote_tracking_refs_before_and_after_fetch() {
        let tmp = TempDir::new().unwrap();
        let remote = tmp.path().join("remote.git");
        let source = tmp.path().join("source");
        let local = tmp.path().join("local");

        run_git(&["init", "--bare", remote.to_str().unwrap()], None);
        fs::create_dir_all(&source).unwrap();
        run_git(&["init"], Some(&source));
        run_git(&["config", "user.name", "test"], Some(&source));
        run_git(&["config", "user.email", "test@test.com"], Some(&source));
        fs::write(source.join("file.txt"), "c1\n").unwrap();
        run_git(&["add", "file.txt"], Some(&source));
        run_git(&["commit", "-m", "c1"], Some(&source));
        run_git(&["branch", "-M", "main"], Some(&source));
        run_git(
            &["remote", "add", "origin", remote.to_str().unwrap()],
            Some(&source),
        );
        run_git(&["push", "-u", "origin", "main"], Some(&source));
        run_git(
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "symbolic-ref",
                "HEAD",
                "refs/heads/main",
            ],
            None,
        );
        run_git(
            &["clone", remote.to_str().unwrap(), local.to_str().unwrap()],
            None,
        );
        let c1 = git2::Oid::from_str(&run_git(
            &["rev-parse", "refs/remotes/origin/main"],
            Some(&local),
        ))
        .unwrap();

        fs::write(source.join("file.txt"), "c2\n").unwrap();
        run_git(&["add", "file.txt"], Some(&source));
        run_git(&["commit", "-m", "c2"], Some(&source));
        run_git(&["push", "origin", "main"], Some(&source));
        let c2 = git2::Oid::from_str(&run_git(&["rev-parse", "refs/heads/main"], Some(&source)))
            .unwrap();

        let repo = Repository {
            id: None,
            path: local.to_string_lossy().to_string(),
            root_path: tmp.path().to_string_lossy().to_string(),
            name: "local".to_string(),
            depth: 1,
            branch: Some("main".to_string()),
            dirty: false,
            file_changes: Vec::new(),
            dirty_files: Vec::new(),
            upstream_ref: Some("origin/main".to_string()),
            upstream_url: Some(remote.to_string_lossy().to_string()),
            ahead_count: 0,
            behind_count: 1,
            freshness: crate::models::Freshness::HasUpdates,
            last_commit_at: None,
            last_commit_message: None,
            last_commit_author: None,
            last_scanned_at: None,
            last_fetch_at: None,
            last_pull_at: None,
        };

        let results = Fetcher::new(1, 10)
            .with_security_scan(false)
            .with_move_to_needauth(false)
            .with_auto_sync(false)
            .fetch_all_detailed(&[repo], false)
            .await;

        assert_eq!(results.len(), 1);
        assert!(
            results[0].success,
            "fetch should succeed, got {:?}",
            results[0].error
        );

        let archive_oids = list_remote_archive_oids(&local);
        assert!(
            archive_oids.contains(&c1),
            "fetch 前归档应保护旧的 origin/main"
        );
        assert!(
            archive_oids.contains(&c2),
            "fetch 后归档应保护新的 origin/main"
        );

        let local_repo = git2::Repository::open(&local).unwrap();
        let latest = local_repo
            .find_reference("refs/glr-remote-archive-latest/origin/main")
            .unwrap()
            .target()
            .unwrap();
        assert_eq!(latest, c2, "latest 归档引用应指向 fetch 后看到的新 HEAD");
    }

    /// Helper: create a minimal git repo with one commit
    fn init_git_repo(path: &std::path::Path) {
        let repo = git2::Repository::init(path).expect("init git repo");
        let sig = git2::Signature::now("test", "test@test.com").expect("create signature");
        let tree_id = {
            let mut index = repo.index().expect("get index");
            index.write_tree().expect("write tree")
        };
        let tree = repo.find_tree(tree_id).expect("find tree");
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .expect("create commit");
    }

    #[test]
    fn move_repo_skips_when_already_at_target() {
        let tmp = TempDir::new().unwrap();
        let needauth_dir = tmp.path().join("needauth");
        let repo_dir = needauth_dir.join("test-repo");
        fs::create_dir_all(&repo_dir).unwrap();
        init_git_repo(&repo_dir);

        let target = needauth_dir.join("test-repo");

        // should not panic, should not delete contents
        Fetcher::move_repo_to_needauth(repo_dir.to_str().unwrap(), &target, &needauth_dir, None)
            .unwrap();

        assert!(repo_dir.exists(), "repo directory must still exist");
        assert!(
            repo_dir.join(".git").exists(),
            ".git directory must still exist"
        );
    }

    #[test]
    fn move_repo_successfully_relocates_from_outside() {
        let tmp = TempDir::new().unwrap();
        let source_dir = tmp.path().join("repos").join("my-repo");
        fs::create_dir_all(&source_dir).unwrap();
        init_git_repo(&source_dir);

        let test_marker = source_dir.join("test-file.txt");
        fs::write(&test_marker, "hello").unwrap();

        let target = tmp.path().join("needauth").join("my-repo");

        Fetcher::move_repo_to_needauth(
            source_dir.to_str().unwrap(),
            &target,
            target.parent().unwrap_or(tmp.path()),
            None,
        )
        .unwrap();

        assert!(!source_dir.exists(), "source should be gone after move");
        assert!(target.exists(), "target should exist after move");
        assert!(target.join(".git").exists(), ".git should be at target");
        assert!(
            target.join("test-file.txt").exists(),
            "content should be at target"
        );
    }

    #[test]
    fn move_repo_overwrites_existing_target() {
        let tmp = TempDir::new().unwrap();
        let source_dir = tmp.path().join("repos").join("repo");
        fs::create_dir_all(&source_dir).unwrap();
        init_git_repo(&source_dir);

        let target = tmp.path().join("needauth").join("repo");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("stale.txt"), "stale").unwrap();

        Fetcher::move_repo_to_needauth(
            source_dir.to_str().unwrap(),
            &target,
            target.parent().unwrap_or(tmp.path()),
            None,
        )
        .unwrap();

        assert!(target.exists());
        assert!(target.join(".git").exists());
        assert!(
            !target.join("stale.txt").exists(),
            "old target content must be gone"
        );
    }
}
