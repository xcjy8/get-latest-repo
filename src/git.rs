use anyhow::Context;
use colored::Colorize;
use git2::{BranchType, Repository as GitRepository, StatusOptions};
use std::path::Path;

use crate::error::{GetLatestRepoError, Result};
use crate::models::{Freshness, Repository};

/// Proxy configuration
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Whether proxy is enabled
    pub enabled: bool,
    /// HTTP proxy address
    pub http_proxy: String,
    /// HTTPS proxy address
    pub https_proxy: String,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            http_proxy: crate::utils::DEFAULT_PROXY_URL.to_string(),
            https_proxy: crate::utils::DEFAULT_PROXY_URL.to_string(),
        }
    }
}

/// Fetch result types
#[derive(Debug, Clone)]
pub enum FetchStatus {
    /// Success
    Success,
    /// Authentication required (401/403)
    AuthenticationRequired { message: String },
    /// Repository not found/private (404)
    RepositoryNotFound { message: String },
    /// Network/timeout errors
    NetworkError { message: String },
    /// Other errors
    OtherError { message: String },
}

impl FetchStatus {
    /// Whether to move to needauth directory
    pub fn should_move_to_needauth(&self) -> bool {
        matches!(
            self,
            FetchStatus::AuthenticationRequired { .. } | FetchStatus::RepositoryNotFound { .. }
        )
    }

    /// Get error message
    pub fn error_message(&self) -> Option<String> {
        match self {
            FetchStatus::Success => None,
            FetchStatus::AuthenticationRequired { message } => {
                Some(format!("需要认证 (401/403): {}", message))
            }
            FetchStatus::RepositoryNotFound { message } => {
                Some(format!("仓库不存在或已转为私有 (404): {}", message))
            }
            FetchStatus::NetworkError { message } => Some(format!("网络错误: {}", message)),
            FetchStatus::OtherError { message } => Some(format!("错误: {}", message)),
        }
    }
}

/// Git operations wrapper
pub struct GitOps {
    proxy: ProxyConfig,
}

impl GitOps {
    /// Create instance with proxy
    pub fn with_proxy(proxy: ProxyConfig) -> Self {
        Self { proxy }
    }

    /// Open repository
    pub fn open(path: &Path) -> Result<GitRepository> {
        GitRepository::open(path).map_err(|e| GetLatestRepoError::OpenRepo {
            path: path.display().to_string(),
            source: e,
        })
    }

    /// Check if path is a Git repository
    pub fn is_repository(path: &Path) -> bool {
        GitRepository::open(path).is_ok()
    }

    /// Get repository info
    pub fn inspect(path: &Path, root_path: &str) -> Result<Repository> {
        let repo = Self::open(path)?;
        let path_str = path.to_string_lossy().to_string();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        // Calculate depth relative to root_path
        let depth = path
            .strip_prefix(root_path)
            .map(|p| p.components().count() as u32)
            .unwrap_or(0);

        // Get current branch
        let branch = Self::get_current_branch(&repo)?;

        // Check local changes (get detailed info)
        let (dirty, file_changes) = Self::check_dirty(&repo)?;
        // Also generate a path list for database compatibility
        let dirty_files: Vec<String> = file_changes.iter().map(|fc| fc.path.clone()).collect();

        // Get upstream info (sanitize URL to remove credentials before storage)
        let (upstream_ref, upstream_url) = Self::get_upstream_info(&repo)?;
        let upstream_url = upstream_url.map(|u| crate::utils::sanitize_url(&u));

        // Calculate ahead/behind
        let (ahead_count, behind_count, freshness) = Self::calculate_sync_status(&repo)?;

        // Get last commit info
        let (last_commit_at, last_commit_message, last_commit_author) =
            Self::get_last_commit_info(&repo)?;

        Ok(Repository {
            id: None,
            path: path_str,
            root_path: root_path.to_string(),
            name,
            depth,
            branch,
            dirty,
            file_changes,
            dirty_files,
            upstream_ref,
            upstream_url,
            ahead_count,
            behind_count,
            freshness,
            last_commit_at,
            last_commit_message,
            last_commit_author,
            last_scanned_at: Some(chrono::Local::now()),
            last_fetch_at: None,
            last_pull_at: None,
        })
    }

    /// Get current branch name
    fn get_current_branch(repo: &GitRepository) -> Result<Option<String>> {
        let head = match repo.head() {
            Ok(head) => head,
            Err(_) => return Ok(None),
        };

        if let Some(name) = head.shorthand() {
            return Ok(Some(name.to_string()));
        }

        Ok(None)
    }

    /// 从当前分支的上游跟踪引用解析远程名称
    ///
    /// 例如：对于上游引用 `refs/remotes/origin/main`，返回 `"origin"`。
    /// 如果分支没有上游跟踪引用，返回 `None`（调用方可回退到默认 `"origin"`）。
    pub(crate) fn get_remote_name(repo: &GitRepository) -> Result<Option<String>> {
        let branch = match Self::get_current_branch(repo)? {
            Some(b) => b,
            None => return Ok(None),
        };

        let local_branch = match repo.find_branch(&branch, BranchType::Local) {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };

        let upstream = match local_branch.upstream() {
            Ok(u) => u,
            Err(_) => return Ok(None),
        };

        let upstream_ref_str = upstream.get().name().unwrap_or_default();

        if upstream_ref_str.starts_with("refs/remotes/") {
            let parts: Vec<&str> = upstream_ref_str.split('/').collect();
            if parts.len() >= 3 {
                return Ok(Some(parts[2].to_string()));
            }
        }

        Ok(None)
    }

    /// 归档当前可见的所有远程跟踪分支 HEAD。
    ///
    /// 目标是尽量长期留存“曾经 fetch 到、且曾经被远程分支引用过”的提交。
    /// 普通 `refs/remotes/<remote>/<branch>` 会在下一次 fetch 时被远程状态覆盖；
    /// 如果远程之后 force-push、删分支或删库，旧提交可能只剩 dangling object，
    /// 最终有被 Git GC 清理的风险。这里把远程跟踪分支当前指向的 OID 复制到
    /// 稳定的本地归档引用中；调用方可以在 fetch 前后各执行一次，分别保护旧
    /// tracking ref 和新 tracking ref。
    ///
    /// - `refs/glr-remote-archive/<remote>/<branch>/<timestamp>-<oid>`：历史时间点快照。
    /// - `refs/glr-remote-archive-latest/<remote>/<branch>`：最近一次看到的分支 HEAD。
    ///
    /// 若 latest 已经指向同一个 OID，则跳过，避免每天重复创建无意义引用。
    pub fn archive_remote_tracking_refs(path: &Path) -> Result<usize> {
        let repo = Self::open(path)?;
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let mut archived = 0usize;

        let refs = repo.references_glob("refs/remotes/*")?;
        for reference in refs {
            let reference = reference?;
            let Some(ref_name) = reference.name() else {
                continue;
            };
            let Some(relative_name) = ref_name.strip_prefix("refs/remotes/") else {
                continue;
            };

            // `origin/HEAD` 通常只是指向默认分支的符号引用，归档它会重复占位；
            // 真正需要长期保护的是具体远程分支，如 `origin/main`。
            let Some((remote_name, branch_name)) = relative_name.split_once('/') else {
                continue;
            };
            if branch_name == "HEAD" {
                continue;
            }

            let Some(oid) = reference.target() else {
                continue;
            };

            let latest_ref = format!(
                "refs/glr-remote-archive-latest/{}/{}",
                remote_name, branch_name
            );
            if repo
                .find_reference(&latest_ref)
                .ok()
                .and_then(|r| r.target())
                == Some(oid)
            {
                continue;
            }

            let oid_hex = oid.to_string();
            let archive_ref = format!(
                "refs/glr-remote-archive/{}/{}/{}-{}",
                remote_name,
                branch_name,
                timestamp,
                &oid_hex[..12]
            );
            repo.reference(
                &archive_ref,
                oid,
                true,
                &format!("getlatestrepo: archive fetched remote {}", relative_name),
            )?;
            repo.reference(
                &latest_ref,
                oid,
                true,
                &format!("getlatestrepo: latest fetched remote {}", relative_name),
            )?;
            archived += 1;
        }

        Ok(archived)
    }

    /// Check for uncommitted local changes (returns detailed change info)
    fn check_dirty(repo: &GitRepository) -> Result<(bool, Vec<crate::models::FileChange>)> {
        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .renames_head_to_index(true)
            .renames_index_to_workdir(true);

        let statuses = repo.statuses(Some(&mut opts))?;
        let mut file_changes = Vec::new();

        for entry in statuses.iter() {
            if let Some(path) = entry.path() {
                let status = entry.status();

                // Determine change type
                let status_str = if status.contains(git2::Status::WT_NEW)
                    || status.contains(git2::Status::INDEX_NEW)
                {
                    "added"
                } else if status.contains(git2::Status::WT_DELETED)
                    || status.contains(git2::Status::INDEX_DELETED)
                {
                    "deleted"
                } else if status.contains(git2::Status::WT_RENAMED)
                    || status.contains(git2::Status::INDEX_RENAMED)
                {
                    "renamed"
                } else if status.contains(git2::Status::WT_TYPECHANGE)
                    || status.contains(git2::Status::INDEX_TYPECHANGE)
                {
                    "typechange"
                } else if status.contains(git2::Status::WT_MODIFIED)
                    || status.contains(git2::Status::INDEX_MODIFIED)
                {
                    "modified"
                } else if status.contains(git2::Status::IGNORED) {
                    "ignored"
                } else {
                    "unknown"
                };

                let staged = status.intersects(
                    git2::Status::INDEX_NEW
                        | git2::Status::INDEX_MODIFIED
                        | git2::Status::INDEX_DELETED
                        | git2::Status::INDEX_RENAMED
                        | git2::Status::INDEX_TYPECHANGE,
                );

                file_changes.push(crate::models::FileChange::new(
                    path.to_string(),
                    status_str,
                    staged,
                ));
            }
        }

        let is_dirty = !file_changes.is_empty();
        Ok((is_dirty, file_changes))
    }

    /// Get upstream info
    fn get_upstream_info(repo: &GitRepository) -> Result<(Option<String>, Option<String>)> {
        let branch = match Self::get_current_branch(repo)? {
            Some(b) => b,
            None => return Ok((None, None)),
        };

        let local_branch = match repo.find_branch(&branch, BranchType::Local) {
            Ok(b) => b,
            Err(_) => return Ok((None, None)),
        };

        let upstream = match local_branch.upstream() {
            Ok(u) => u,
            Err(_) => return Ok((None, None)),
        };

        let upstream_ref = upstream.name()?.map(|s| s.to_string());

        // Get remote URL
        let upstream_ref_str = upstream
            .get()
            .name()
            .map(|s| s.to_string())
            .unwrap_or_default();

        let upstream_url = if upstream_ref_str.starts_with("refs/remotes/") {
            let parts: Vec<&str> = upstream_ref_str.split('/').collect();
            if parts.len() >= 3 {
                let remote_name = parts[2];
                repo.find_remote(remote_name)
                    .ok()
                    .and_then(|r| r.url().map(|u| u.to_string()))
            } else {
                None
            }
        } else {
            None
        };

        Ok((upstream_ref, upstream_url))
    }

    /// Calculate sync status
    fn calculate_sync_status(repo: &GitRepository) -> Result<(i32, i32, Freshness)> {
        let local_branch = match Self::get_current_branch(repo)? {
            Some(b) => b,
            None => return Ok((0, 0, Freshness::NoRemote)),
        };

        let branch = match repo.find_branch(&local_branch, BranchType::Local) {
            Ok(b) => b,
            Err(_) => return Ok((0, 0, Freshness::NoRemote)),
        };

        let upstream = match branch.upstream() {
            Ok(u) => u,
            Err(_) => return Ok((0, 0, Freshness::NoRemote)),
        };

        let local_ref = branch.get().target();
        let upstream_ref = upstream.get().target();

        let (local_oid, upstream_oid) = match (local_ref, upstream_ref) {
            (Some(local), Some(upstream)) => (local, upstream),
            _ => return Ok((0, 0, Freshness::NoRemote)),
        };

        // Calculate ahead/behind
        let (ahead, behind) = repo.graph_ahead_behind(local_oid, upstream_oid)?;

        let freshness = if behind > 0 {
            Freshness::HasUpdates
        } else {
            Freshness::Synced
        };

        Ok((ahead as i32, behind as i32, freshness))
    }

    /// Get last commit info
    #[allow(clippy::type_complexity)]
    fn get_last_commit_info(
        repo: &GitRepository,
    ) -> Result<(
        Option<chrono::DateTime<chrono::Local>>,
        Option<String>,
        Option<String>,
    )> {
        let head = match repo.head() {
            Ok(head) => head,
            Err(_) => return Ok((None, None, None)),
        };

        let oid = match head.target() {
            Some(oid) => oid,
            None => return Ok((None, None, None)),
        };

        let commit = match repo.find_commit(oid) {
            Ok(c) => c,
            Err(_) => return Ok((None, None, None)),
        };

        let time = commit.time();
        let dt = chrono::DateTime::from_timestamp(time.seconds(), 0).and_then(|dt| {
            chrono::FixedOffset::east_opt(time.offset_minutes() * 60)
                .map(|offset| dt.with_timezone(&offset).with_timezone(&chrono::Local))
        });

        let message = commit.message().map(|m| m.trim().to_string());

        let author = commit.author().name().map(|n| n.to_string());

        Ok((dt, message, author))
    }

    /// 使用原生 git 命令执行 fetch（兜底路径）
    ///
    /// 当 git2 因认证、代理或网络配置问题失败时，使用原生 git 命令兜底。
    /// 原生 git 会读取 ~/.ssh/config、使用 ssh-agent、支持 credential-helper，
    /// 且可以通过 child.kill() 在超时后强制终止。
    fn fetch_with_git_command(&self, path: &Path, timeout_secs: u64) -> FetchStatus {
        let remote_name = match Self::open(path) {
            Ok(repo) => Self::get_remote_name(&repo)
                .ok()
                .flatten()
                .unwrap_or_else(|| "origin".to_string()),
            Err(_) => "origin".to_string(),
        };

        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C")
            .arg(path)
            .args(["fetch", &remote_name])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_HTTP_LOW_SPEED_TIME", "10")
            .env("GIT_HTTP_LOW_SPEED_LIMIT", "1000")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());

        // 使用环境变量传递代理，兼容旧版本 git（不支持 `git -c`）
        if self.proxy.enabled {
            cmd.env("HTTP_PROXY", &self.proxy.http_proxy)
                .env("HTTPS_PROXY", &self.proxy.https_proxy)
                .env("ALL_PROXY", &self.proxy.http_proxy);
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                return FetchStatus::OtherError {
                    message: format!("无法启动 git fetch: {e}"),
                };
            }
        };

        // Drain stderr in a background thread to prevent pipe buffer deadlock
        let stderr_handle = child.stderr.take().map(|mut err| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(&mut err, &mut buf);
                buf
            })
        });

        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(timeout_secs);

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let stderr_buf = stderr_handle
                        .and_then(|h| h.join().ok())
                        .unwrap_or_default();
                    let stderr = String::from_utf8_lossy(&stderr_buf);

                    if status.success() {
                        return FetchStatus::Success;
                    }

                    let exit_code = status.code().unwrap_or(-1);
                    let error_msg =
                        format!("git fetch 失败（退出码 {exit_code}）: {}", stderr.trim());
                    return Self::classify_error(&error_msg);
                }
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return FetchStatus::NetworkError {
                            message: format!("超时 ({} 秒)", timeout_secs),
                        };
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => {
                    return FetchStatus::OtherError {
                        message: format!("等待 git fetch 结束失败: {e}"),
                    };
                }
            }
        }
    }

    /// 对外接口：使用原生 git 命令执行 fetch
    ///
    /// 使用原生 git 而非 git2 的原因（v0.1.5 验证结论）：
    /// 1. 原生 git 支持 SSH agent、credential-helper、~/.ssh/config 等完整认证链
    /// 2. git2 在认证兼容性上有局限（不支持 credential-helper、部分 SSH 配置）
    /// 3. 原生 git 可通过 child.kill() 在超时后强制终止，行为更可预测
    pub fn fetch_detailed(&self, path: &Path, timeout_secs: u64) -> (FetchStatus, Option<String>) {
        let status = self.fetch_with_git_command(path, timeout_secs);
        (status, None)
    }

    /// Classify error type
    fn classify_error(error_msg: &str) -> FetchStatus {
        let msg = error_msg.to_lowercase();

        // Rate limiting (must be checked before auth — GitHub returns 403 for rate limits)
        if msg.contains("rate limit") || msg.contains("too many requests") || msg.contains("429") {
            return FetchStatus::NetworkError {
                message: format!("触发速率限制: {}", error_msg),
            };
        }

        // Authentication-related errors (403 excluded here — it could be rate limiting)
        if msg.contains("401")
            || msg.contains("authentication")
            || msg.contains("credentials")
            || msg.contains("authorization")
            || msg.contains("unauthorized")
        {
            return FetchStatus::AuthenticationRequired {
                message: error_msg.to_string(),
            };
        }

        // 网络/超时错误。这里同时覆盖 git2 和原生 `git fetch`
        // 可能返回的 curl/libcurl/OpenSSL/DNS 文案。
        if msg.contains("timeout")
            || msg.contains("timed out")
            || msg.contains("connection refused")
            || msg.contains("couldn't connect")
            || msg.contains("could not resolve")
            || msg.contains("couldn't resolve")
            || msg.contains("network")
            || msg.contains("unreachable")
            || msg.contains("unable to access")
            || msg.contains("rpc failed")
            || msg.contains("curl")
            || msg.contains("openssl")
            || msg.contains("operation timed out")
            || msg.contains("failed to connect")
        {
            return FetchStatus::NetworkError {
                message: error_msg.to_string(),
            };
        }

        // 404 repository not found. DNS 解析失败必须在网络错误分支优先处理，
        // 否则临时网络问题会被误判为私有/不存在仓库并触发 needauth 移动。
        if msg.contains("404") || msg.contains("not found") || msg.contains("repository not found")
        {
            return FetchStatus::RepositoryNotFound {
                message: error_msg.to_string(),
            };
        }

        // 其他无法归类的错误保留原始信息，交给上层展示。
        FetchStatus::OtherError {
            message: error_msg.to_string(),
        }
    }
}

/// Pull force execution results
#[derive(Debug, Clone)]
pub enum PullForceOutcome {
    /// Success (clean repository directly pulled, or dirty repository stash-pull-pop succeeded)
    Success,
    /// Stash pop conflict (pull succeeded, but pop failed)
    Conflict {
        /// Stash message
        stash_name: String,
        /// Conflict file list
        conflict_files: Vec<String>,
        /// Stash index in stash list (e.g., stash@{2})
        stash_index: Option<usize>,
    },
}

impl GitOps {
    /// Safe pull: fast-forward only for clean repositories
    ///
    /// Precondition checks:
    /// - Repository must exist
    /// - Must have a current branch
    /// - Remote branch must exist
    /// - Local must be clean (guaranteed by caller)
    pub fn pull_ff_only(path: &Path) -> Result<()> {
        let repo = Self::open(path)?;

        let branch = Self::get_current_branch(&repo)?;
        let branch_name = match branch {
            Some(b) => b,
            None => return Err(crate::error::GetLatestRepoError::DetachedHead),
        };

        // 动态解析远程名称，避免硬编码 "origin"
        let remote_name = Self::get_remote_name(&repo)?.unwrap_or_else(|| "origin".to_string());

        // Check if remote branch exists
        let remote_branch = format!("{}/{}", remote_name, branch_name);
        let remote_ref_name = format!("refs/remotes/{}", remote_branch);

        let remote_ref = match repo.find_reference(&remote_ref_name) {
            Ok(r) => r,
            Err(_) => return Err(crate::error::GetLatestRepoError::RemoteBranchMissing),
        };

        let remote_oid = remote_ref.target().ok_or_else(|| {
            GetLatestRepoError::Other(anyhow::anyhow!(
                "无法获取远程分支 '{}' 的 OID",
                remote_branch
            ))
        })?;

        // Get local branch reference
        let local_ref_name = format!("refs/heads/{}", branch_name);
        let mut local_ref = repo.find_reference(&local_ref_name).map_err(|e| {
            GetLatestRepoError::Other(anyhow::anyhow!("无法找到本地分支 '{}': {}", branch_name, e))
        })?;

        // Fast-forward merge
        let remote_obj = repo.find_object(remote_oid, None).map_err(|e| {
            GetLatestRepoError::Other(anyhow::anyhow!("无法找到远程提交对象: {}", e))
        })?;

        // Save original OID for potential rollback
        let original_oid = local_ref
            .target()
            .ok_or_else(|| GetLatestRepoError::Other(anyhow::anyhow!("无法获取当前分支 OID")))?;

        // Verify this is actually a fast-forward (local is ancestor of remote)
        let (ahead, behind) = repo
            .graph_ahead_behind(original_oid, remote_oid)
            .map_err(|e| {
                GetLatestRepoError::Other(anyhow::anyhow!("计算 ahead/behind 失败: {}", e))
            })?;
        if ahead > 0 {
            if behind > 0 {
                return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                    "无法快进合并：分支已分叉，本地领先 {} 个提交，落后 {} 个提交",
                    ahead,
                    behind
                )));
            } else {
                return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                    "无法快进合并：本地分支有 {} 个未推送的提交",
                    ahead
                )));
            }
        }

        // Update ref first, then checkout. If checkout fails, rollback the ref.
        // This ensures the ref always points to a valid commit.
        local_ref
            .set_target(remote_oid, "pull-safe: fast-forward")
            .map_err(|e| {
                GetLatestRepoError::Other(anyhow::anyhow!("更新本地分支引用失败: {}", e))
            })?;

        // 显式使用 SAFE 策略，避免依赖 libgit2 默认行为
        let mut checkout_opts = git2::build::CheckoutBuilder::new();
        checkout_opts.safe();

        if let Err(e) = repo.checkout_tree(&remote_obj, Some(&mut checkout_opts)) {
            // Rollback: restore ref to original OID
            if let Err(e2) =
                local_ref.set_target(original_oid, "pull-safe: rollback failed checkout")
            {
                return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                    "严重错误：检出失败 ({}), 回滚引用也失败 ({})。仓库可能处于不一致状态。",
                    e,
                    e2
                )));
            }
            // Restore working directory to original state (force 策略确保回滚成功)
            let original_obj = repo.find_object(original_oid, None).map_err(|e3| {
                GetLatestRepoError::Other(anyhow::anyhow!(
                    "严重错误：无法找到原始提交对象用于工作目录恢复: {}",
                    e3
                ))
            })?;
            let mut rollback_opts = git2::build::CheckoutBuilder::new();
            rollback_opts.force();
            if let Err(e3) = repo.checkout_tree(&original_obj, Some(&mut rollback_opts)) {
                return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                    "严重错误：检出失败 ({}), 引用已回滚但工作目录恢复也失败 ({})。仓库可能处于不一致状态。",
                    e,
                    e3
                )));
            }
            return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                "检出远程变更失败: {}。分支引用和工作目录已恢复至原始状态。",
                e
            )));
        }

        // 二次验证：检出后工作区是否真正同步
        let (is_dirty_after, _) = Self::check_dirty(&repo)?;
        if is_dirty_after {
            // Auto-repair：尝试强制重置到 HEAD
            let head = repo.head().map_err(|e| {
                GetLatestRepoError::Other(anyhow::anyhow!(
                    "Fast-forward 后检测到工作区残留差异，但获取 HEAD 失败: {}",
                    e
                ))
            })?;
            let head_commit = head.peel_to_commit().map_err(|e| {
                GetLatestRepoError::Other(anyhow::anyhow!(
                    "Fast-forward 后检测到工作区残留差异，但解析 HEAD 提交失败: {}",
                    e
                ))
            })?;
            let mut hard_opts = git2::build::CheckoutBuilder::new();
            hard_opts.force();
            if let Err(e) = repo.reset(
                head_commit.as_object(),
                git2::ResetType::Hard,
                Some(&mut hard_opts),
            ) {
                // hard-reset 失败，回滚 ref
                if let Err(e2) = local_ref
                    .set_target(original_oid, "pull-safe: rollback after hard-reset failed")
                {
                    return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                        "严重错误：检出不完整且强制重置失败 ({}), 回滚引用也失败 ({})。仓库可能处于不一致状态。",
                        e,
                        e2
                    )));
                }
                return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                    "检出远程变更不完整，强制重置失败: {}。分支引用已恢复至原始状态。",
                    e
                )));
            }

            // 再次验证
            let (still_dirty, _) = Self::check_dirty(&repo)?;
            if still_dirty {
                if let Err(e2) = local_ref.set_target(
                    original_oid,
                    "pull-safe: rollback after checkout incomplete",
                ) {
                    return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                        "严重错误：检出不完整且强制重置后仍有差异，回滚引用也失败 ({})。仓库可能处于不一致状态。",
                        e2
                    )));
                }
                return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                    "检出远程变更不完整：强制重置后工作区仍存在差异。分支引用已恢复至原始状态。",
                )));
            }
        }

        Ok(())
    }

    /// Force pull: stash → pull → pop
    /// Returns PullForceOutcome
    pub fn pull_force(path: &Path) -> Result<PullForceOutcome> {
        let mut repo = Self::open(path)?;
        let stash_name = format!(
            "getlatestrepo-before-pull-{}",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        );

        // Check for local changes
        let (is_dirty, _) = Self::check_dirty(&repo)?;
        let stash_created = if is_dirty {
            // 1. Stash local changes
            let sig = repo.signature()?;
            repo.stash_save(&sig, &stash_name, Some(git2::StashFlags::INCLUDE_UNTRACKED))?;
            true
        } else {
            false
        };

        // 2. Pull (ff-only, safest)
        let pull_result = (|| -> Result<()> {
            let branch = Self::get_current_branch(&repo)?;
            let branch_name = match branch {
                Some(name) => name,
                None => return Err(GetLatestRepoError::DetachedHead),
            };
            {
                let remote_name =
                    Self::get_remote_name(&repo)?.unwrap_or_else(|| "origin".to_string());
                let remote_branch = format!("{}/{}", remote_name, branch_name);
                let remote_ref = repo.find_reference(&format!("refs/remotes/{}", remote_branch))?;
                let remote_oid = remote_ref.target().context("无法获取远程分支 OID")?;

                let mut local_ref = repo.find_reference(&format!("refs/heads/{}", branch_name))?;

                // Save original OID for potential rollback
                let original_oid = local_ref.target().ok_or_else(|| {
                    GetLatestRepoError::Other(anyhow::anyhow!("无法获取当前分支 OID"))
                })?;

                // 安全检查：验证是否为 fast-forward，防止丢失本地未推送的提交
                let (ahead, behind) =
                    repo.graph_ahead_behind(original_oid, remote_oid)
                        .map_err(|e| {
                            GetLatestRepoError::Other(anyhow::anyhow!(
                                "计算 ahead/behind 失败: {}",
                                e
                            ))
                        })?;
                if ahead > 0 {
                    if behind > 0 {
                        return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                            "无法快进合并：分支已分叉，本地领先 {} 个提交，落后 {} 个提交。请先处理本地提交后再执行 pull-force",
                            ahead,
                            behind
                        )));
                    } else {
                        return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                            "无法快进合并：本地分支有 {} 个未推送的提交。请先推送或处理本地提交后再执行 pull-force",
                            ahead
                        )));
                    }
                }

                // Update ref first, then checkout. If checkout fails, rollback the ref.
                let remote_obj = repo.find_object(remote_oid, None)?;
                local_ref
                    .set_target(remote_oid, "pull-force: fast-forward")
                    .map_err(|e| {
                        GetLatestRepoError::Other(anyhow::anyhow!("更新本地分支引用失败: {}", e))
                    })?;

                let mut checkout_opts = git2::build::CheckoutBuilder::new();
                checkout_opts.safe();
                if let Err(e) = repo.checkout_tree(&remote_obj, Some(&mut checkout_opts)) {
                    // Rollback: restore ref to original OID
                    if let Err(e2) =
                        local_ref.set_target(original_oid, "pull-force: rollback failed checkout")
                    {
                        return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                            "严重错误：检出失败 ({}), 回滚引用也失败 ({})。仓库可能处于不一致状态。",
                            e,
                            e2
                        )));
                    }
                    return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                        "检出远程变更失败: {}。分支引用已恢复至原始状态。",
                        e
                    )));
                }

                // 二次验证：检出后工作区是否真正同步
                let (is_dirty_after, _) = Self::check_dirty(&repo)?;
                if is_dirty_after {
                    let head = repo.head().map_err(|e| {
                        GetLatestRepoError::Other(anyhow::anyhow!(
                            "Fast-forward 后检测到工作区残留差异，但获取 HEAD 失败: {}",
                            e
                        ))
                    })?;
                    let head_commit = head.peel_to_commit().map_err(|e| {
                        GetLatestRepoError::Other(anyhow::anyhow!(
                            "Fast-forward 后检测到工作区残留差异，但解析 HEAD 提交失败: {}",
                            e
                        ))
                    })?;
                    let mut hard_opts = git2::build::CheckoutBuilder::new();
                    hard_opts.force();
                    if let Err(e) = repo.reset(
                        head_commit.as_object(),
                        git2::ResetType::Hard,
                        Some(&mut hard_opts),
                    ) {
                        if let Err(e2) = local_ref.set_target(
                            original_oid,
                            "pull-force: rollback after hard-reset failed",
                        ) {
                            return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                                "严重错误：检出不完整且强制重置失败 ({}), 回滚引用也失败 ({})。仓库可能处于不一致状态。",
                                e,
                                e2
                            )));
                        }
                        return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                            "检出远程变更不完整，强制重置失败: {}。分支引用已恢复至原始状态。",
                            e
                        )));
                    }
                    let (still_dirty, _) = Self::check_dirty(&repo)?;
                    if still_dirty {
                        if let Err(e2) = local_ref.set_target(
                            original_oid,
                            "pull-force: rollback after checkout incomplete",
                        ) {
                            return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                                "严重错误：检出不完整且强制重置后仍有差异，回滚引用也失败 ({})。仓库可能处于不一致状态。",
                                e2
                            )));
                        }
                        return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                            "检出远程变更不完整：强制重置后工作区仍存在差异。分支引用已恢复至原始状态。",
                        )));
                    }
                }
            }
            Ok(())
        })();

        match pull_result {
            Ok(()) => {
                // 3. If stash exists, attempt pop
                if stash_created {
                    match repo.stash_pop(0, None) {
                        Ok(()) => Ok(PullForceOutcome::Success),
                        Err(_) => {
                            // Pop failed, collect conflict details for manual resolution
                            let conflict_files = Self::get_conflict_files(&mut repo);
                            let stash_index = Self::find_stash_index(&mut repo, &stash_name);
                            Ok(PullForceOutcome::Conflict {
                                stash_name,
                                conflict_files,
                                stash_index,
                            })
                        }
                    }
                } else {
                    Ok(PullForceOutcome::Success)
                }
            }
            Err(e) => {
                // Pull failed after stash was created — warn user about the orphan stash
                if stash_created {
                    eprintln!("   ⚠️ Pull 失败，但本地变更已保存到 stash: {}", stash_name);
                    eprintln!("      可用以下命令手动恢复: git stash pop stash@{{0}}");
                }
                Err(e)
            }
        }
    }

    /// Get conflicted files after a failed stash pop
    fn get_conflict_files(repo: &mut git2::Repository) -> Vec<String> {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(false);
        match repo.statuses(Some(&mut opts)) {
            Ok(statuses) => statuses
                .iter()
                .filter(|entry| entry.status().contains(git2::Status::CONFLICTED))
                .filter_map(|entry| entry.path().map(|s| s.to_string()))
                .collect(),
            Err(e) => {
                eprintln!("警告：获取冲突文件失败: {}", e);
                Vec::new()
            }
        }
    }

    /// 判断仓库是否处于未合并索引状态。
    ///
    /// 未合并索引通常来自上一次 stash pop / merge / checkout 冲突。libgit2 无法
    /// 对这种 index 创建 stash tree，会报 `cannot create a tree from a not fully merged index`。
    /// `pull-backup` 的语义是镜像远程，因此这里把它作为“需要强制重置清理”的状态。
    fn has_unmerged_index(repo: &git2::Repository) -> bool {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(false);
        match repo.statuses(Some(&mut opts)) {
            Ok(statuses) => statuses
                .iter()
                .any(|entry| entry.status().contains(git2::Status::CONFLICTED)),
            Err(e) => {
                eprintln!("警告：检查未合并索引失败: {}", e);
                false
            }
        }
    }

    /// 按 stash message 查找对应的 stash 序号。
    fn find_stash_index(repo: &mut git2::Repository, stash_name: &str) -> Option<usize> {
        let mut result = None;
        if let Err(e) = repo.stash_foreach(|index, message, _oid| {
            if message == stash_name {
                result = Some(index);
                false // 找到目标后立即停止遍历，避免继续扫描无关 stash。
            } else {
                true
            }
        }) {
            eprintln!("警告：遍历 stash 失败: {}", e);
        }
        result
    }

    /// 备份同步：归档 → 必要时 stash → 硬重置到远程 → 恢复 stash。
    ///
    /// 这个流程面向“本地只做镜像备份”的仓库：用户不在本地维护业务修改，
    /// 只希望本地副本尽量严格匹配远程。它会自动处理两类常见风险：
    /// - 远程历史被改写（force push / rebase）：先把旧 HEAD 归档到 refs。
    /// - 本地存在未提交变更：先 stash，再 reset，最后尝试 stash pop。
    ///
    /// 返回 `(PullForceOutcome, Option<archive_ref_name>)`。当远程历史改写会丢弃
    /// 本地 HEAD 时，`archive_ref_name` 会指向 `refs/glr-archive/history/<branch>/<timestamp>`，
    /// 方便用户事后审计或恢复旧历史。
    pub fn pull_backup(path: &Path) -> Result<(PullForceOutcome, Option<String>)> {
        let mut repo = Self::open(path)?;

        let branch = Self::get_current_branch(&repo)?;
        let branch_name = match branch {
            Some(name) => name,
            None => return Err(GetLatestRepoError::DetachedHead),
        };

        // 0. 如果硬重置会丢失本地历史，先创建归档引用。
        let archive_ref = Self::maybe_archive_before_reset(&mut repo, &branch_name)?;

        // 1. 只在普通 dirty 状态下保存本地变更；未合并索引无法创建 stash tree，
        //    对备份模式来说应直接进入硬重置恢复路径。
        let (is_dirty, _) = Self::check_dirty(&repo)?;
        let has_unmerged_index = Self::has_unmerged_index(&repo);
        let stash_name = if is_dirty && !has_unmerged_index {
            let name = format!(
                "getlatestrepo-backup-{}",
                chrono::Local::now().format("%Y%m%d-%H%M%S")
            );
            let sig = repo.signature()?;
            match repo.stash_save(&sig, &name, Some(git2::StashFlags::INCLUDE_UNTRACKED)) {
                Ok(_) => Some(name),
                Err(e) if Self::is_empty_stash_error(&e) => {
                    eprintln!("   ℹ️ 检测到 dirty 状态但没有可 stash 的内容，继续执行备份同步。");
                    None
                }
                Err(e) => return Err(e.into()),
            }
        } else {
            if has_unmerged_index {
                eprintln!("   ⚠️ 检测到未合并索引，备份模式将跳过 stash 并硬重置到远程。");
            }
            None
        };

        // 2. 硬重置到远程跟踪分支；这一步负责处理普通落后和分叉历史。
        let reset_result = (|| -> Result<()> {
            let remote_name = Self::get_remote_name(&repo)?.unwrap_or_else(|| "origin".to_string());
            let remote_ref_name = format!("refs/remotes/{}/{}", remote_name, branch_name);

            let remote_ref = repo
                .find_reference(&remote_ref_name)
                .map_err(|_| GetLatestRepoError::RemoteBranchMissing)?;
            let remote_oid = remote_ref
                .target()
                .ok_or_else(|| GetLatestRepoError::RemoteBranchNoTarget)?;

            Self::reset_hard_to_remote(&repo, path, &remote_ref_name, remote_oid)?;

            Ok(())
        })();

        match reset_result {
            Ok(()) => {
                // 3. 如果第 1 步创建了 stash，尝试恢复；冲突会被显式返回给上层展示。
                if let Some(stash_name) = stash_name {
                    match repo.stash_pop(0, None) {
                        Ok(()) => Ok((PullForceOutcome::Success, archive_ref)),
                        Err(_) => {
                            let conflict_files = Self::get_conflict_files(&mut repo);
                            let stash_index = Self::find_stash_index(&mut repo, &stash_name);
                            Ok((
                                PullForceOutcome::Conflict {
                                    stash_name,
                                    conflict_files,
                                    stash_index,
                                },
                                archive_ref,
                            ))
                        }
                    }
                } else {
                    Ok((PullForceOutcome::Success, archive_ref))
                }
            }
            Err(e) => {
                if stash_name.is_some() {
                    eprintln!("   ⚠️ 硬重置失败，但本地修改已保存到 stash。请手动恢复。");
                }
                Err(e)
            }
        }
    }

    /// 判断 stash 失败是否只是“没有可保存内容”。
    ///
    /// 部分仓库会被 status 判为 dirty，但 libgit2 在真正创建 stash 时发现没有
    /// 可写入的变更。对 `pull-backup` 来说，这不应阻断后续 hard reset。
    fn is_empty_stash_error(error: &git2::Error) -> bool {
        error.code() == git2::ErrorCode::NotFound
            && error.class() == git2::ErrorClass::Stash
            && error.message().contains("nothing to stash")
    }

    /// 将本地分支硬重置到 upstream tracking ref。
    ///
    /// 优先使用 libgit2，保持和其他 Git 操作一致。若 libgit2 在检出超长 symlink
    /// 时失败，则回退到原生 git，并临时禁用 symlink 检出，把 symlink 写成普通文件。
    /// 这是为了兼容备份仓库中存在异常长 symlink target 的情况。
    fn reset_hard_to_remote(
        repo: &git2::Repository,
        path: &Path,
        remote_ref_name: &str,
        remote_oid: git2::Oid,
    ) -> Result<()> {
        let remote_obj = repo.find_object(remote_oid, None).map_err(|e| {
            GetLatestRepoError::Other(anyhow::anyhow!("无法找到远程提交对象: {}", e))
        })?;

        let mut hard_opts = git2::build::CheckoutBuilder::new();
        hard_opts.force();
        match repo.reset(&remote_obj, git2::ResetType::Hard, Some(&mut hard_opts)) {
            Ok(()) => Ok(()),
            Err(e) if Self::is_symlink_filename_too_long_error(&e) => {
                Self::reset_hard_with_native_git_no_symlinks(path, remote_ref_name)
            }
            Err(e) => Err(GetLatestRepoError::Other(anyhow::anyhow!(
                "硬重置到远程分支失败: {}",
                e
            ))),
        }
    }

    /// 判断 hard reset 失败是否来自超长 symlink target。
    ///
    /// 这类错误不是仓库认证、网络或提交对象问题，而是当前文件系统无法创建
    /// 目标过长的符号链接。备份模式可以安全地回退到 `core.symlinks=false`。
    fn is_symlink_filename_too_long_error(error: &git2::Error) -> bool {
        let message = error.message();
        message.contains("could not create symlink") && message.contains("File name too long")
    }

    /// 使用原生 git 执行禁用 symlink 的硬重置回退。
    ///
    /// libgit2 没有等价于临时 `-c core.symlinks=false` 的 checkout 开关；
    /// 因此只在明确识别出超长 symlink 错误后调用原生 git。该命令只修改目标
    /// 仓库工作区，不触碰远程，也不会修改本项目仓库历史。
    fn reset_hard_with_native_git_no_symlinks(path: &Path, remote_ref_name: &str) -> Result<()> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args([
                "-c",
                "core.symlinks=false",
                "reset",
                "--hard",
                remote_ref_name,
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|e| {
                GetLatestRepoError::Other(anyhow::anyhow!("无法启动原生 git 回退重置: {}", e))
            })?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(GetLatestRepoError::Other(anyhow::anyhow!(
            "硬重置到远程分支失败，且禁用 symlink 的原生 git 回退也失败: {}",
            stderr.trim()
        )))
    }

    /// Check if hard reset would lose local branch history, and create archive ref if so.
    ///
    /// Creates two refs when archiving:
    /// - refs/glr-archive/history/<branch>/<timestamp>  (point-in-time snapshot)
    /// - refs/glr-archive/latest/<branch>               (always points to most recent archive)
    ///
    /// Returns the archive ref name if created, None if not needed (fast-forward or same commit).
    fn maybe_archive_before_reset(
        repo: &mut git2::Repository,
        branch_name: &str,
    ) -> Result<Option<String>> {
        let head_oid = match repo.head()?.target() {
            Some(oid) => oid,
            None => return Ok(None),
        };

        let remote_name = Self::get_remote_name(repo)?.unwrap_or_else(|| "origin".to_string());
        let remote_ref_name = format!("refs/remotes/{}/{}", remote_name, branch_name);
        let remote_oid = match repo
            .find_reference(&remote_ref_name)
            .ok()
            .and_then(|r| r.target())
        {
            Some(oid) => oid,
            None => return Ok(None),
        };

        // Same commit: no update needed, no archive needed
        if head_oid == remote_oid {
            return Ok(None);
        }

        // Check if local HEAD is an ancestor of remote HEAD.
        // graph_ahead_behind(head, remote):
        //   ahead  = commits in head not in remote
        //   behind = commits in remote not in head
        // If ahead == 0: head is ancestor of remote (fast-forward), no archive needed.
        // If ahead > 0:  head is NOT ancestor, history would be lost.
        // If Err:        unrelated histories, archive needed.
        let needs_archive = match repo.graph_ahead_behind(head_oid, remote_oid) {
            Ok((ahead, _)) => ahead > 0,
            Err(_) => true, // unrelated histories (complete rewrite)
        };

        if needs_archive {
            let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            // 分支名本身可以包含 `/`。把历史快照和 latest 放进固定子命名空间，
            // 避免 `feature`、`feature/x`、`feature-latest` 等名字与归档后缀混淆。
            let archive_ref = format!("refs/glr-archive/history/{}/{}", branch_name, timestamp);
            let latest_ref = format!("refs/glr-archive/latest/{}", branch_name);

            repo.reference(
                &archive_ref,
                head_oid,
                true,
                &format!("getlatestrepo: archive {} before pull-backup", branch_name),
            )?;
            repo.reference(
                &latest_ref,
                head_oid,
                true,
                &format!("getlatestrepo: latest archive for {}", branch_name),
            )?;

            Ok(Some(archive_ref))
        } else {
            Ok(None)
        }
    }

    /// Get recent N commits (used to display new commits after pull)
    pub fn get_recent_commits(path: &Path, count: usize) -> Result<Vec<String>> {
        let repo = Self::open(path)?;
        let mut commits = Vec::new();

        let mut revwalk = repo.revwalk()?;
        revwalk.push_head()?;

        for oid in revwalk.take(count) {
            let oid = oid?;
            let commit = repo.find_commit(oid)?;

            let msg = commit
                .message()
                .map(|m| m.lines().next().unwrap_or(m).to_string())
                .unwrap_or_else(|| "(no message)".to_string());

            let oid_str = oid.to_string();
            let short_id = if oid_str.len() >= 7 {
                &oid_str[..7]
            } else {
                &oid_str
            };
            commits.push(format!("{} {}", short_id.dimmed(), msg));
        }

        Ok(commits)
    }

    /// 获取从指定 OID（不包含）到 HEAD 的所有提交
    ///
    /// 用于 `diff_after` 精确显示本次 pull 新增的提交，而非 HEAD 最近 N 条。
    pub fn get_commits_since(path: &Path, since_oid: git2::Oid) -> Result<Vec<String>> {
        let repo = Self::open(path)?;
        let head_oid = repo
            .head()?
            .target()
            .ok_or_else(|| GetLatestRepoError::Other(anyhow::anyhow!("HEAD 没有目标提交")))?;

        // 如果 since_oid 就是 HEAD，说明没有新提交
        if since_oid == head_oid {
            return Ok(Vec::new());
        }

        let mut commits = Vec::new();
        let mut revwalk = repo.revwalk()?;
        revwalk.push_head()?;

        for oid_result in revwalk {
            let oid = oid_result?;
            // 到达 since_oid 时停止（不包含 since_oid）
            if oid == since_oid {
                break;
            }
            let commit = repo.find_commit(oid)?;
            let msg = commit
                .message()
                .map(|m| m.lines().next().unwrap_or(m).to_string())
                .unwrap_or_else(|| "(no message)".to_string());
            let oid_str = oid.to_string();
            let short_id = if oid_str.len() >= 7 {
                &oid_str[..7]
            } else {
                &oid_str
            };
            commits.push(format!("{} {}", short_id.dimmed(), msg));
        }

        Ok(commits)
    }

    /// Discard all local changes (git restore .)
    ///
    /// # Warning
    /// This operation will permanently lose all uncommitted changes, including:
    /// - Working directory changes
    /// - Staged changes  
    /// - Untracked files (if include_untracked=true)
    ///
    /// # Parameters
    /// - `path`: Repository path
    /// - `include_untracked`: Whether to also delete untracked files
    pub fn discard_changes(path: &Path, include_untracked: bool) -> Result<Vec<String>> {
        let repo = Self::open(path)?;

        // Get current status to return the list of discarded files
        let mut status_opts = git2::StatusOptions::new();
        status_opts.include_untracked(include_untracked);
        let statuses = repo.statuses(Some(&mut status_opts))?;
        let mut discarded_files = Vec::new();

        for entry in statuses.iter() {
            if let Some(path) = entry.path() {
                discarded_files.push(path.to_string());
            }
        }

        // Get HEAD tree
        let head = repo.head()?;
        let head_tree = head.peel_to_tree()?;

        // Execute checkout to restore working directory to HEAD state
        let mut checkout_opts = git2::build::CheckoutBuilder::new();
        checkout_opts
            .force()
            .remove_untracked(include_untracked)
            .remove_ignored(false);

        repo.checkout_tree(head_tree.as_object(), Some(&mut checkout_opts))?;

        // Reset staging area
        let head_commit = head.peel_to_commit()?;
        repo.reset(head_commit.as_object(), git2::ResetType::Mixed, None)?;

        Ok(discarded_files)
    }

    /// Check remote repository for anomalies (detect deletion or emptying)
    ///
    /// Use `graph_ahead_behind` O(1) comparison instead of revwalk counting,
    /// detecting whether remote history was force-pushed back.
    pub fn check_pull_safety(path: &Path) -> Result<PullSafetyReport> {
        let repo = Self::open(path)?;

        let branch = Self::get_current_branch(&repo)?;
        let branch_name = match branch {
            Some(b) => b,
            None => return Err(crate::error::GetLatestRepoError::DetachedHead),
        };

        // Get current remote HEAD
        let remote_name = Self::get_remote_name(&repo)?.unwrap_or_else(|| "origin".to_string());
        let remote_ref_name = format!("refs/remotes/{}/{}", remote_name, branch_name);
        let current_oid = match repo.find_reference(&remote_ref_name) {
            Ok(r) => match r.target() {
                Some(oid) => oid,
                None => return Err(crate::error::GetLatestRepoError::RemoteBranchNoTarget),
            },
            Err(_) => {
                return Ok(PullSafetyReport {
                    is_safe: false,
                    remote_commits: 0,
                    previous_remote_commits: 0,
                    change_ratio: 0.0,
                    warning: Some("远程分支不存在，请先运行 fetch".to_string()),
                    details: vec![],
                });
            }
        };

        // Get previous remote OID from reflog at last fetch
        let previous_oid = Self::previous_remote_oid(&repo, &remote_ref_name);

        let mut details = vec![];

        if let Some(prev_oid) = previous_oid {
            if prev_oid == current_oid {
                // No changes
                return Ok(PullSafetyReport {
                    is_safe: true,
                    remote_commits: 0,
                    previous_remote_commits: 0,
                    change_ratio: 0.0,
                    warning: None,
                    details: vec!["远程无新提交".to_string()],
                });
            }

            // O(1) ahead/behind comparison
            let (ahead, behind) = repo.graph_ahead_behind(current_oid, prev_oid)?;
            details.push(format!("新增 {} 个提交，丢失 {} 个提交", ahead, behind));

            if behind > 0 && behind > ahead {
                // Remote history regression
                let total = ahead + behind;
                let regression_ratio = if total > 0 {
                    behind as f64 / total as f64
                } else {
                    1.0
                };

                if regression_ratio > 0.5 {
                    return Ok(PullSafetyReport {
                        is_safe: false,
                        remote_commits: ahead,
                        previous_remote_commits: behind + ahead,
                        change_ratio: -regression_ratio,
                        warning: Some(format!(
                            "检测到疑似仓库删除！远程历史回退：丢失 {} 个提交，仅新增 {} 个提交",
                            behind, ahead,
                        )),
                        details,
                    });
                } else if regression_ratio > 0.2 {
                    return Ok(PullSafetyReport {
                        is_safe: true,
                        remote_commits: ahead,
                        previous_remote_commits: behind + ahead,
                        change_ratio: -regression_ratio,
                        warning: Some(format!(
                            "远程提交数减少：丢失 {} 个提交，新增 {} 个提交",
                            behind, ahead,
                        )),
                        details,
                    });
                }
            }

            // ahead > behind -> normal forward
            if ahead > 0 {
                details.push(format!("远程新增 {} 个提交（正常更新）", ahead));
            }

            Ok(PullSafetyReport {
                is_safe: true,
                remote_commits: ahead,
                previous_remote_commits: ahead + behind,
                change_ratio: 0.0,
                warning: None,
                details,
            })
        } else {
            // No reflog, cannot compare — treat as safe but with a warning
            details.push("首次获取，无历史数据用于比对".to_string());
            Ok(PullSafetyReport {
                is_safe: true,
                remote_commits: 0,
                previous_remote_commits: 0,
                change_ratio: 0.0,
                warning: Some("首次获取 — 无基准数据，无法检测异常".to_string()),
                details,
            })
        }
    }

    /// Get previous remote OID from reflog at last fetch
    ///
    /// Entry 0's `id_old()` is the state before the most recent update.
    /// Falls back to searching older entries if entry 0's `id_old()` is zero.
    fn previous_remote_oid(repo: &GitRepository, ref_name: &str) -> Option<git2::Oid> {
        let reflog = repo.reflog(ref_name).ok()?;
        if reflog.is_empty() {
            return None;
        }
        let current_oid = reflog.get(0)?.id_new();
        // Entry 0's id_old() is the state before the most recent fetch
        let old_oid = reflog.get(0)?.id_old();
        if !old_oid.is_zero() && old_oid != current_oid {
            return Some(old_oid);
        }
        // Fallback: search older entries for any non-current OID
        for i in 1..reflog.len() {
            let entry = reflog.get(i)?;
            let new_oid = entry.id_new();
            if !new_oid.is_zero() && new_oid != current_oid {
                return Some(new_oid);
            }
        }
        None
    }
}

/// Pull safety check report
///
/// Note: some fields are currently only used for debugging, reserved for future detailed reporting
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PullSafetyReport {
    /// Whether safe (can pull)
    pub is_safe: bool,
    /// Number of new commits on remote (ahead of local)
    pub remote_commits: usize,
    /// Total commits involved in the change (ahead + behind)
    pub previous_remote_commits: usize,
    /// Change ratio (reserved for debugging)
    pub change_ratio: f64,
    /// Warning message (if any)
    pub warning: Option<String>,
    /// Detailed description (reserved for detailed reporting)
    pub details: Vec<String>,
}

/// Format time difference into human-readable format
pub fn format_duration(dt: &Option<chrono::DateTime<chrono::Local>>) -> String {
    match dt {
        Some(dt) => {
            let now = chrono::Local::now();
            let duration = now.signed_duration_since(*dt);

            if duration.num_minutes() < 1 {
                "刚刚".to_string()
            } else if duration.num_hours() < 1 {
                format!("{} 分钟前", duration.num_minutes())
            } else if duration.num_days() < 1 {
                format!("{} 小时前", duration.num_hours())
            } else if duration.num_days() < 30 {
                format!("{} 天前", duration.num_days())
            } else if duration.num_days() < 365 {
                format!("{} 个月前", duration.num_days() / 30)
            } else {
                format!("{} 年前", duration.num_days() / 365)
            }
        }
        None => "-".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Helper: create a repo with main branch and origin/main tracking ref at same commit
    fn create_repo_with_tracking() -> (TempDir, std::path::PathBuf, git2::Oid) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().to_path_buf();
        let repo = git2::Repository::init(&path).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();

        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let c1 = repo
            .commit(Some("HEAD"), &sig, &sig, "c1", &tree, &[])
            .unwrap();

        repo.branch("main", &repo.find_commit(c1).unwrap(), false)
            .unwrap();
        repo.set_head("refs/heads/main").unwrap();
        repo.reference("refs/remotes/origin/main", c1, true, "tracking")
            .unwrap();

        (tmp, path, c1)
    }

    /// Helper: add a commit on current branch (HEAD)
    fn add_commit(path: &std::path::Path, message: &str) -> git2::Oid {
        let repo = git2::Repository::open(path).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        let parent = repo.head().unwrap().target().unwrap();
        let parent_commit = repo.find_commit(parent).unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent_commit])
            .unwrap()
    }

    /// Helper: add a commit from a specific parent (detached, not on HEAD)
    fn add_commit_from_parent(
        path: &std::path::Path,
        parent_oid: git2::Oid,
        message: &str,
    ) -> git2::Oid {
        let repo = git2::Repository::open(path).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        let parent_commit = repo.find_commit(parent_oid).unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(None, &sig, &sig, message, &tree, &[&parent_commit])
            .unwrap()
    }

    /// Helper: move refs/remotes/origin/main to target commit
    fn move_tracking_ref(path: &std::path::Path, target_oid: git2::Oid) {
        let repo = git2::Repository::open(path).unwrap();
        repo.reference(
            "refs/remotes/origin/main",
            target_oid,
            true,
            "update tracking",
        )
        .unwrap();
    }

    #[test]
    fn test_pull_ff_only_behind_remote_succeeds() {
        let (_tmp, path, c1) = create_repo_with_tracking();
        let c2 = add_commit_from_parent(&path, c1, "remote commit");
        move_tracking_ref(&path, c2);
        let result = GitOps::pull_ff_only(&path);
        assert!(
            result.is_ok(),
            "Expected success when local is behind remote, got: {:?}",
            result
        );
    }

    #[test]
    fn test_pull_ff_only_up_to_date_succeeds() {
        let (_tmp, path, _c1) = create_repo_with_tracking();
        let result = GitOps::pull_ff_only(&path);
        assert!(
            result.is_ok(),
            "Expected success when up to date, got: {:?}",
            result
        );
    }

    #[test]
    fn test_pull_ff_only_ahead_of_remote_fails() {
        let (_tmp, path, _c1) = create_repo_with_tracking();
        add_commit(&path, "local commit");
        let result = GitOps::pull_ff_only(&path);
        assert!(
            result.is_err(),
            "Expected failure when local is ahead of remote"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("无法快进合并")
                || err_msg.contains("分叉")
                || err_msg.contains("未推送"),
            "Error message should mention fast-forward or diverged, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_pull_ff_only_diverged_fails() {
        let (_tmp, path, c1) = create_repo_with_tracking();
        add_commit(&path, "local commit");
        let c2_remote = add_commit_from_parent(&path, c1, "remote commit");
        move_tracking_ref(&path, c2_remote);
        let result = GitOps::pull_ff_only(&path);
        assert!(result.is_err(), "Expected failure when branches diverged");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("无法快进合并")
                || err_msg.contains("分叉")
                || err_msg.contains("未推送"),
            "Error message should mention fast-forward or diverged, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_pull_force_behind_remote_succeeds() {
        let (_tmp, path, c1) = create_repo_with_tracking();
        let c2 = add_commit_from_parent(&path, c1, "remote commit");
        move_tracking_ref(&path, c2);
        let result = GitOps::pull_force(&path);
        assert!(
            result.is_ok(),
            "Expected success when local is behind remote, got: {:?}",
            result
        );
    }

    #[test]
    fn test_pull_force_up_to_date_succeeds() {
        let (_tmp, path, _c1) = create_repo_with_tracking();
        let result = GitOps::pull_force(&path);
        assert!(
            result.is_ok(),
            "Expected success when up to date, got: {:?}",
            result
        );
    }

    #[test]
    fn test_pull_force_ahead_of_remote_fails() {
        let (_tmp, path, _c1) = create_repo_with_tracking();
        add_commit(&path, "local commit");
        let result = GitOps::pull_force(&path);
        assert!(
            result.is_err(),
            "Expected failure when local is ahead of remote"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("无法快进合并") || err_msg.contains("未推送"),
            "Error message should mention fast-forward, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_pull_force_diverged_fails() {
        let (_tmp, path, c1) = create_repo_with_tracking();
        add_commit(&path, "local commit");
        let c2_remote = add_commit_from_parent(&path, c1, "remote commit");
        move_tracking_ref(&path, c2_remote);
        let result = GitOps::pull_force(&path);
        assert!(result.is_err(), "Expected failure when branches diverged");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("无法快进合并")
                || err_msg.contains("分叉")
                || err_msg.contains("未推送"),
            "Error message should mention fast-forward or diverged, got: {}",
            err_msg
        );
    }

    /// 验证 checkout_tree SAFE 策略跳过冲突文件后，auto-repair 能强制同步
    #[test]
    fn test_pull_ff_only_auto_repair_skipped_files() {
        let (_tmp, path, c1) = create_repo_with_tracking();
        let repo = git2::Repository::open(&path).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();

        // 提交 B：添加 new.txt
        let new_file_path = path.join("new.txt");
        std::fs::write(&new_file_path, "remote content").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("new.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let c1_commit = repo.find_commit(c1).unwrap();
        let c2 = repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                "add new file",
                &tree,
                &[&c1_commit],
            )
            .unwrap();

        // 回退 HEAD 到 A（模拟本地落后）
        repo.reset(c1_commit.as_object(), git2::ResetType::Hard, None)
            .unwrap();
        assert!(
            !new_file_path.exists(),
            "new.txt should be removed after reset to A"
        );

        // 设置远程引用指向 B
        move_tracking_ref(&path, c2);

        // 在工作区创建一个未跟踪的 new.txt（与 B 中的内容不同）
        // 这会触发 checkout_tree SAFE 策略跳过 new.txt 的检出
        std::fs::write(&new_file_path, "untracked local content").unwrap();

        // 调用 pull_ff_only：checkout_tree 会跳过 new.txt，但 auto-repair 应强制同步
        let result = GitOps::pull_ff_only(&path);
        assert!(
            result.is_ok(),
            "Expected success with auto-repair, got: {:?}",
            result
        );

        // 验证工作区是干净的
        let repo_after = git2::Repository::open(&path).unwrap();
        let (is_dirty, _) = GitOps::check_dirty(&repo_after).unwrap();
        assert!(
            !is_dirty,
            "Expected clean working directory after auto-repair"
        );

        // 验证 new.txt 的内容与 B 中的一致
        let content = std::fs::read_to_string(&new_file_path).unwrap();
        assert_eq!(
            content, "remote content",
            "new.txt should be updated to remote version after auto-repair"
        );

        // 验证 HEAD 指向 B
        let head_oid = repo_after.head().unwrap().target().unwrap();
        assert_eq!(
            head_oid, c2,
            "HEAD should point to remote commit after pull"
        );
    }

    // ==================== Pull-backup archive tests ====================

    /// Helper: list all refs under refs/glr-archive/
    fn list_archive_refs(path: &std::path::Path) -> Vec<String> {
        let repo = match git2::Repository::open(path) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut refs = Vec::new();
        if let Ok(refs_iter) = repo.references_glob("refs/glr-archive/*") {
            for r in refs_iter.flatten() {
                if let Some(name) = r.name() {
                    refs.push(name.to_string());
                }
            }
        }
        refs
    }

    /// Helper: list all refs under refs/glr-remote-archive/
    fn list_remote_archive_refs(path: &std::path::Path) -> Vec<(String, git2::Oid)> {
        let repo = match git2::Repository::open(path) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut refs = Vec::new();
        if let Ok(refs_iter) = repo.references_glob("refs/glr-remote-archive/*") {
            for r in refs_iter.flatten() {
                if let (Some(name), Some(oid)) = (r.name(), r.target()) {
                    refs.push((name.to_string(), oid));
                }
            }
        }
        refs
    }

    #[test]
    fn test_archive_remote_tracking_refs_preserves_seen_remote_heads() {
        let (_tmp, path, c1) = create_repo_with_tracking();

        let first_count = GitOps::archive_remote_tracking_refs(&path).unwrap();
        assert_eq!(first_count, 1, "首次看到 origin/main 应创建归档引用");

        let duplicate_count = GitOps::archive_remote_tracking_refs(&path).unwrap();
        assert_eq!(duplicate_count, 0, "同一个远程 HEAD 不应重复创建归档引用");

        let c2 = add_commit_from_parent(&path, c1, "remote rewrite target");
        move_tracking_ref(&path, c2);

        let second_count = GitOps::archive_remote_tracking_refs(&path).unwrap();
        assert_eq!(second_count, 1, "远程 HEAD 改变后应创建新的归档引用");

        let repo = git2::Repository::open(&path).unwrap();
        let latest = repo
            .find_reference("refs/glr-remote-archive-latest/origin/main")
            .unwrap()
            .target()
            .unwrap();
        assert_eq!(
            latest, c2,
            "latest 引用应指向最近一次 fetch 后看到的远程 HEAD"
        );

        let archive_refs = list_remote_archive_refs(&path);
        assert!(
            archive_refs.iter().any(|(_, oid)| *oid == c1),
            "第一次看到的远程 HEAD 必须被历史归档引用保护"
        );
        assert!(
            archive_refs.iter().any(|(_, oid)| *oid == c2),
            "第二次看到的远程 HEAD 必须被历史归档引用保护"
        );
    }

    #[test]
    fn test_pull_backup_fast_forward_no_archive() {
        let (_tmp, path, c1) = create_repo_with_tracking();
        // Remote advances: c1 -> c2
        let c2 = add_commit_from_parent(&path, c1, "remote commit");
        move_tracking_ref(&path, c2);

        // HEAD is at c1, remote is at c2: fast-forward scenario
        let result = GitOps::pull_backup(&path);
        assert!(result.is_ok(), "Expected success, got: {:?}", result);

        let (outcome, archive_ref) = result.unwrap();
        assert!(matches!(outcome, PullForceOutcome::Success));
        assert!(
            archive_ref.is_none(),
            "Fast-forward should not create archive ref"
        );

        // Verify no archive refs exist
        let archives = list_archive_refs(&path);
        assert!(
            archives.is_empty(),
            "Should have no archive refs for fast-forward"
        );

        // Verify HEAD moved to c2
        let repo = git2::Repository::open(&path).unwrap();
        let head_oid = repo.head().unwrap().target().unwrap();
        assert_eq!(head_oid, c2, "HEAD should point to remote commit");
    }

    #[test]
    fn test_pull_backup_force_push_creates_archive() {
        let (_tmp, path, c1) = create_repo_with_tracking();
        // Local advances: c1 -> c2_local
        let c2_local = add_commit(&path, "local commit");

        // Remote force-pushed to a different branch: c1 -> c3_remote
        let c3_remote = add_commit_from_parent(&path, c1, "remote rewritten commit");
        move_tracking_ref(&path, c3_remote);

        // HEAD is at c2_local, remote is at c3_remote: diverged, needs archive
        let result = GitOps::pull_backup(&path);
        assert!(result.is_ok(), "Expected success, got: {:?}", result);

        let (outcome, archive_ref) = result.unwrap();
        assert!(matches!(outcome, PullForceOutcome::Success));
        assert!(
            archive_ref.is_some(),
            "Force-push should create archive ref"
        );

        // Verify archive refs exist
        let archives = list_archive_refs(&path);
        assert!(
            !archives.is_empty(),
            "Should have archive refs after force-push"
        );
        assert!(
            archives
                .iter()
                .any(|r| r.starts_with("refs/glr-archive/history/main/")),
            "Should have main history archive ref"
        );
        assert!(
            archives.iter().any(|r| r == "refs/glr-archive/latest/main"),
            "Should have main latest ref"
        );

        // Verify HEAD moved to c3_remote
        let repo = git2::Repository::open(&path).unwrap();
        let head_oid = repo.head().unwrap().target().unwrap();
        assert_eq!(
            head_oid, c3_remote,
            "HEAD should point to new remote commit"
        );

        // Verify old history is still accessible via archive ref
        let archive_ref_name = archive_ref.unwrap();
        let archive_oid = repo
            .find_reference(&archive_ref_name)
            .unwrap()
            .target()
            .unwrap();
        assert_eq!(
            archive_oid, c2_local,
            "Archive ref should point to old local HEAD"
        );
    }

    #[test]
    fn test_pull_backup_archives_slash_branch_without_ref_conflict() {
        let (_tmp, path, c1) = create_repo_with_tracking();
        let repo = git2::Repository::open(&path).unwrap();

        repo.reference("refs/heads/feature/demo", c1, true, "create slash branch")
            .unwrap();
        repo.set_head("refs/heads/feature/demo").unwrap();
        repo.reference(
            "refs/remotes/origin/feature/demo",
            c1,
            true,
            "create slash tracking",
        )
        .unwrap();
        drop(repo);

        let local_oid = add_commit(&path, "local slash branch commit");
        let remote_oid = add_commit_from_parent(&path, c1, "remote slash branch rewrite");
        let repo = git2::Repository::open(&path).unwrap();
        repo.reference(
            "refs/remotes/origin/feature/demo",
            remote_oid,
            true,
            "rewrite slash tracking",
        )
        .unwrap();
        drop(repo);

        let (outcome, archive_ref) = GitOps::pull_backup(&path).unwrap();
        assert!(matches!(outcome, PullForceOutcome::Success));

        let archive_ref_name = archive_ref.expect("slash branch should create archive ref");
        assert!(
            archive_ref_name.starts_with("refs/glr-archive/history/feature/demo/"),
            "archive ref should keep slash branch under history namespace"
        );

        let repo = git2::Repository::open(&path).unwrap();
        let archive_oid = repo
            .find_reference(&archive_ref_name)
            .unwrap()
            .target()
            .unwrap();
        assert_eq!(
            archive_oid, local_oid,
            "archive ref should preserve old local HEAD"
        );

        let latest_oid = repo
            .find_reference("refs/glr-archive/latest/feature/demo")
            .unwrap()
            .target()
            .unwrap();
        assert_eq!(
            latest_oid, local_oid,
            "latest archive ref should preserve old local HEAD"
        );
    }

    #[test]
    fn test_pull_backup_up_to_date_no_archive() {
        let (_tmp, path, _c1) = create_repo_with_tracking();
        // Already up to date
        let result = GitOps::pull_backup(&path);
        assert!(result.is_ok(), "Expected success, got: {:?}", result);

        let (outcome, archive_ref) = result.unwrap();
        assert!(matches!(outcome, PullForceOutcome::Success));
        assert!(
            archive_ref.is_none(),
            "Up-to-date should not create archive ref"
        );

        let archives = list_archive_refs(&path);
        assert!(
            archives.is_empty(),
            "Should have no archive refs when up to date"
        );
    }

    #[test]
    fn test_pull_backup_recovers_unmerged_index() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path();

        let run_git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .output()
                .expect("git command should start");
            assert!(
                output.status.success(),
                "git {:?} should succeed\nstdout: {}\nstderr: {}",
                args,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };

        run_git(&["init"]);
        run_git(&["config", "user.email", "test@test.com"]);
        run_git(&["config", "user.name", "Test"]);
        std::fs::write(path.join("conflict.txt"), "base\n").unwrap();
        run_git(&["add", "conflict.txt"]);
        run_git(&["commit", "-m", "base"]);
        run_git(&["branch", "-M", "main"]);

        run_git(&["checkout", "-b", "remote-work"]);
        std::fs::write(path.join("conflict.txt"), "remote\n").unwrap();
        run_git(&["commit", "-am", "remote"]);
        let remote_oid = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let remote_oid = String::from_utf8(remote_oid.stdout).unwrap();
        run_git(&["update-ref", "refs/remotes/origin/main", remote_oid.trim()]);

        run_git(&["checkout", "main"]);
        std::fs::write(path.join("conflict.txt"), "local\n").unwrap();
        run_git(&["commit", "-am", "local"]);

        let merge_output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["merge", "remote-work"])
            .output()
            .unwrap();
        assert!(
            !merge_output.status.success(),
            "merge should create a conflict"
        );

        let result = GitOps::pull_backup(path);
        assert!(
            matches!(result, Ok((PullForceOutcome::Success, Some(_)))),
            "pull-backup should hard reset conflicted backup repo, got: {:?}",
            result
        );

        let repo = git2::Repository::open(path).unwrap();
        let head_oid = repo.head().unwrap().target().unwrap().to_string();
        assert_eq!(head_oid, remote_oid.trim());
        assert_eq!(
            std::fs::read_to_string(path.join("conflict.txt")).unwrap(),
            "remote\n"
        );
    }

    #[test]
    fn test_classify_dns_resolution_as_network_error() {
        let status = GitOps::classify_error(
            "git fetch 失败（退出码 128）: fatal: unable to access 'https://github.com/a/b': Could not resolve host: github.com",
        );

        assert!(
            matches!(status, FetchStatus::NetworkError { .. }),
            "DNS 解析失败必须保持为网络错误，不能触发 needauth 移动"
        );
    }
}
