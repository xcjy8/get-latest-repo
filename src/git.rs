use anyhow::Context;
use colored::Colorize;
use git2::{BranchType, Repository as GitRepository, StatusOptions};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

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
    const AUTOMATION_USER_NAME: &'static str = "GetLatestRepo";
    const AUTOMATION_USER_EMAIL: &'static str = "getlatestrepo@localhost";

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

    /// Fetch 只更新远程跟踪引用，不会修改分支 HEAD 或工作区文件。
    ///
    /// 因此 Fetch 后只重算 upstream 与 ahead/behind；复用已有 dirty、提交信息，
    /// 避免再次遍历整个工作区。若检测到分支被外部进程切换，则回退完整检查，
    /// 保证并发人工操作下的数据正确性。
    pub fn refresh_remote_state_after_fetch(cached: &Repository) -> Result<Repository> {
        let path = Path::new(&cached.path);
        let repo = Self::open(path)?;
        let branch = Self::get_current_branch(&repo)?;
        if branch != cached.branch {
            return Self::inspect(path, &cached.root_path);
        }

        let (upstream_ref, upstream_url) = Self::get_upstream_info(&repo)?;
        let upstream_url = upstream_url.map(|url| crate::utils::sanitize_url(&url));
        let (ahead_count, behind_count, freshness) = Self::calculate_sync_status(&repo)?;
        let mut refreshed = cached.clone();
        refreshed.upstream_ref = upstream_ref;
        refreshed.upstream_url = upstream_url;
        refreshed.ahead_count = ahead_count;
        refreshed.behind_count = behind_count;
        refreshed.freshness = freshness;
        refreshed.last_scanned_at = Some(chrono::Local::now());
        Ok(refreshed)
    }

    /// Get current branch name
    fn get_current_branch(repo: &GitRepository) -> Result<Option<String>> {
        let head = match repo.head() {
            Ok(head) => head,
            Err(_) => return Ok(None),
        };

        if let Ok(name) = head.shorthand() {
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
            let Ok(ref_name) = reference.name() else {
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
            if let Ok(path) = entry.path() {
                let status = entry.status();
                // Finder 浏览目录时会自动写入未跟踪的 `.DS_Store`。它既不代表用户
                // 修改了仓库内容，也会让大量只读镜像仓库被误标为“本地修改”。
                // 仅忽略纯未跟踪状态；已纳入版本控制的 `.DS_Store` 发生变化仍需报告。
                if Self::is_untracked_finder_metadata(repo, path, status) {
                    continue;
                }
                // libgit2 不执行 Git LFS 的 clean filter，会把已经正确水合的大文件
                // 与索引中的 LFS 指针误判为修改。按指针记录的 SHA-256 与大小核验；
                // 只有内容完全一致才忽略，真实改动仍然报告。
                if Self::is_clean_hydrated_lfs_file(repo, path, status) {
                    continue;
                }

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

    fn is_untracked_finder_metadata(
        repo: &GitRepository,
        path: &str,
        status: git2::Status,
    ) -> bool {
        if status != git2::Status::WT_NEW {
            return false;
        }
        let relative_path = Path::new(path);
        if relative_path
            .file_name()
            .is_some_and(|name| name == ".DS_Store")
        {
            return true;
        }
        let Some(workdir) = repo.workdir() else {
            return false;
        };
        let directory = workdir.join(relative_path);
        if !directory.is_dir() {
            return false;
        }

        // libgit2 默认把未跟踪目录折叠成 `目录/`。仅在小目录内逐项确认，
        // 防止 `migration/.DS_Store` 这类 Finder 噪声被误判，同时避免遍历大型产物目录。
        let mut files = 0_usize;
        for entry in walkdir::WalkDir::new(directory).follow_links(false) {
            let Ok(entry) = entry else {
                return false;
            };
            if entry.file_type().is_symlink() {
                return false;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            files += 1;
            if files > 128 || entry.file_name() != ".DS_Store" {
                return false;
            }
        }
        files > 0
    }

    fn is_clean_hydrated_lfs_file(repo: &GitRepository, path: &str, status: git2::Status) -> bool {
        if status != git2::Status::WT_MODIFIED {
            return false;
        }
        let Some(workdir) = repo.workdir() else {
            return false;
        };
        let Ok(index) = repo.index() else {
            return false;
        };
        let Some(entry) = index.get_path(Path::new(path), 0) else {
            return false;
        };
        let Ok(blob) = repo.find_blob(entry.id) else {
            return false;
        };
        let Some((expected_oid, expected_size)) = Self::parse_lfs_pointer(blob.content()) else {
            return false;
        };
        let file_path = workdir.join(path);
        let Ok(metadata) = file_path.metadata() else {
            return false;
        };
        if !metadata.is_file() || metadata.len() != expected_size {
            return false;
        }
        let Ok(mut file) = std::fs::File::open(file_path) else {
            return false;
        };
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let Ok(read) = file.read(&mut buffer) else {
                return false;
            };
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Self::hex_bytes(hasher.finalize().as_ref()) == expected_oid
    }

    fn parse_lfs_pointer(content: &[u8]) -> Option<(String, u64)> {
        if content.len() > 1024 {
            return None;
        }
        let text = std::str::from_utf8(content).ok()?;
        let mut lines = text.lines();
        if lines.next()? != "version https://git-lfs.github.com/spec/v1" {
            return None;
        }
        let oid = lines.next()?.strip_prefix("oid sha256:")?;
        if oid.len() != 64 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let size = lines.next()?.strip_prefix("size ")?.parse().ok()?;
        Some((oid.to_ascii_lowercase(), size))
    }

    fn hex_bytes(bytes: &[u8]) -> String {
        use std::fmt::Write;

        let mut output = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(&mut output, "{byte:02x}").expect("写入 String 不会失败");
        }
        output
    }

    /// Stash 需要作者签名，但容器化只读镜像通常不会配置全局 Git 身份。
    ///
    /// 优先使用用户已有身份；缺失或无效时仅为当前 stash 生成临时自动化签名，
    /// 不写入仓库配置，也不污染宿主机全局配置。
    fn stash_signature(repo: &GitRepository) -> Result<git2::Signature<'static>> {
        match repo.signature() {
            Ok(signature) => Ok(signature),
            Err(_) => git2::Signature::now(Self::AUTOMATION_USER_NAME, Self::AUTOMATION_USER_EMAIL)
                .map_err(Into::into),
        }
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
                    .and_then(|r| r.url().ok().map(str::to_string))
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

        let message = commit.message().ok().map(|m| m.trim().to_string());

        let author = commit.author().name().ok().map(str::to_string);

        Ok((dt, message, author))
    }

    /// 使用原生 git 命令执行 fetch（兜底路径）
    ///
    /// 当 git2 因认证、代理或网络配置问题失败时，使用原生 git 命令兜底。
    /// 原生 git 会读取 ~/.ssh/config、使用 ssh-agent、支持 credential-helper，
    /// 且可以通过 child.kill() 在超时后强制终止。
    fn fetch_with_git_command(
        &self,
        path: &Path,
        timeout_secs: u64,
        cancellation_checker: Option<&(dyn Fn() -> bool + Send + Sync)>,
    ) -> FetchStatus {
        let remote_name = match Self::open(path) {
            Ok(repo) => Self::get_remote_name(&repo)
                .ok()
                .flatten()
                .unwrap_or_else(|| "origin".to_string()),
            Err(_) => "origin".to_string(),
        };

        let mut cmd = std::process::Command::new("git");
        self.configure_fetch_proxy(&mut cmd, &remote_name);
        cmd.arg("-C")
            .arg(path)
            .args(["fetch", &remote_name])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_HTTP_LOW_SPEED_TIME", "10")
            .env("GIT_HTTP_LOW_SPEED_LIMIT", "1000")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());

        Self::configure_process_group(&mut cmd);

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
                    if crate::signal_handler::is_shutdown_requested()
                        || cancellation_checker.is_some_and(|checker| checker())
                    {
                        Self::terminate_process_group(&mut child);
                        let _ = stderr_handle.and_then(|handle| handle.join().ok());
                        return FetchStatus::OtherError {
                            message: "任务已取消".to_string(),
                        };
                    }
                    if start.elapsed() >= timeout {
                        Self::terminate_process_group(&mut child);
                        let _ = stderr_handle.and_then(|handle| handle.join().ok());
                        return FetchStatus::NetworkError {
                            message: format!("超时 ({} 秒)", timeout_secs),
                        };
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => {
                    Self::terminate_process_group(&mut child);
                    let _ = stderr_handle.and_then(|handle| handle.join().ok());
                    return FetchStatus::OtherError {
                        message: format!("等待 git fetch 结束失败: {e}"),
                    };
                }
            }
        }
    }

    /// 使用命令级配置覆盖仓库内遗留代理，确保容器统一走宿主机代理。
    ///
    /// `.git/config` 的 `http.proxy` 会覆盖普通环境变量；容器中的
    /// `127.0.0.1` 又指向容器自身。因此同时覆盖通用代理与 remote 专用代理，
    /// 并保留环境变量供 Git 的底层传输实现读取。
    fn configure_fetch_proxy(&self, cmd: &mut std::process::Command, remote_name: &str) {
        if self.proxy.enabled {
            cmd.arg("-c")
                .arg(format!("http.proxy={}", self.proxy.http_proxy))
                .arg("-c")
                .arg(format!("https.proxy={}", self.proxy.https_proxy))
                .arg("-c")
                .arg(format!(
                    "remote.{remote_name}.proxy={}",
                    self.proxy.https_proxy
                ));
            cmd.env("HTTP_PROXY", &self.proxy.http_proxy)
                .env("HTTPS_PROXY", &self.proxy.https_proxy)
                .env("ALL_PROXY", &self.proxy.http_proxy);
        }
    }

    /// 对外接口：使用原生 git 命令执行 fetch
    ///
    /// 使用原生 git 而非 git2 的原因（v0.1.5 验证结论）：
    /// 1. 原生 git 支持 SSH agent、credential-helper、~/.ssh/config 等完整认证链
    /// 2. git2 在认证兼容性上有局限（不支持 credential-helper、部分 SSH 配置）
    /// 3. 原生 git 可在超时或取消时终止完整进程组，行为更可预测
    ///
    /// 支持上层 Web 操作取消；终止整个 Git 进程组，避免遗留 ssh/credential 子进程。
    pub fn fetch_detailed_with_cancellation(
        &self,
        path: &Path,
        timeout_secs: u64,
        cancellation_checker: Option<&(dyn Fn() -> bool + Send + Sync)>,
    ) -> (FetchStatus, Option<String>) {
        let status = self.fetch_with_git_command(path, timeout_secs, cancellation_checker);
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

        // HTTP 状态码必须先于 libcurl 的通用 “unable to access” 文案判定。
        // 速率限制已在上方优先排除，因此剩余 403 归为认证问题。
        if msg.contains("403") {
            return FetchStatus::AuthenticationRequired {
                message: error_msg.to_string(),
            };
        }
        if msg.contains("404") {
            return FetchStatus::RepositoryNotFound {
                message: error_msg.to_string(),
            };
        }

        // Authentication-related errors
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
        if msg.contains("not found") || msg.contains("repository not found") {
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
    /// Success (operation completed; callers decide whether stash is restored or kept)
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

        // 调用方保存的扫描快照可能已经过期，因此真正修改分支引用前必须再次读取工作区。
        // `pull-safe` 的安全边界属于 Git 操作本身，不能依赖数据库中的历史 dirty 状态。
        let (is_dirty, _) = Self::check_dirty(&repo)?;
        if is_dirty {
            return Err(GetLatestRepoError::DirtyWorkingTree);
        }

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
            let sig = Self::stash_signature(&repo)?;
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
                .filter_map(|entry| entry.path().ok().map(str::to_string))
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

    /// 备份同步：归档 → 必要时 stash 保护本地修改 → 硬重置到远程。
    ///
    /// 这个流程面向“本地只做镜像备份”的仓库：用户不在本地维护业务修改，
    /// 只希望本地副本尽量严格匹配远程。它会自动处理两类常见风险：
    /// - 远程历史被改写（force push / rebase）：先把旧 HEAD 归档到 refs。
    /// - 本地存在未提交变更：先 stash 作为恢复点，再 reset；不会自动 pop，
    ///   否则工作区会重新变 dirty，无法满足“备份副本严格匹配远程”的语义。
    ///
    /// 返回 `(PullForceOutcome, Option<archive_ref_name>)`。当远程历史改写会丢弃
    /// 本地 HEAD 时，`archive_ref_name` 会指向 `refs/glr-archive/history/<branch>/<timestamp>`，
    /// 方便用户事后审计或恢复旧历史。
    pub fn pull_backup(path: &Path) -> Result<(PullForceOutcome, Option<String>)> {
        Self::pull_backup_inner(path, None)
    }

    /// Web 可取消版本；取消只发生在阶段边界或受管原生 Git 子进程内。
    pub fn pull_backup_with_cancellation(
        path: &Path,
        cancellation_checker: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<(PullForceOutcome, Option<String>)> {
        Self::pull_backup_inner(path, Some(cancellation_checker.as_ref()))
    }

    fn pull_backup_inner(
        path: &Path,
        cancellation_checker: Option<&(dyn Fn() -> bool + Send + Sync)>,
    ) -> Result<(PullForceOutcome, Option<String>)> {
        let mut repo = Self::open(path)?;

        Self::ensure_not_cancelled(cancellation_checker)?;

        let branch = Self::get_current_branch(&repo)?;
        let branch_name = match branch {
            Some(name) => name,
            None => return Err(GetLatestRepoError::DetachedHead),
        };

        // 0. 如果硬重置会丢失本地历史，先创建归档引用。
        let archive_ref = Self::maybe_archive_before_reset(&mut repo, &branch_name)?;
        Self::ensure_not_cancelled(cancellation_checker)?;

        // 1. 先保护子模块内部的本地修改。父仓库的 stash/reset 只能处理 gitlink
        //    指针，不能保存子模块自己的工作区；如果这里不单独处理，像
        //    `cmux/ghostty` 这种子模块 dirty 会在父仓库 reset 后继续显示为脏。
        Self::stash_dirty_submodules_for_backup(path, cancellation_checker)?;
        Self::ensure_not_cancelled(cancellation_checker)?;

        // 2. 只在普通 dirty 状态下保存父仓库本地变更；未合并索引无法创建
        //    stash tree，对备份模式来说应直接进入硬重置恢复路径。
        let (is_dirty, _) = Self::check_dirty(&repo)?;
        let has_unmerged_index = Self::has_unmerged_index(&repo);
        let mut needs_no_symlink_reset = false;
        let stash_name = if is_dirty && !has_unmerged_index {
            let name = format!(
                "getlatestrepo-backup-{}",
                chrono::Local::now().format("%Y%m%d-%H%M%S")
            );
            let sig = Self::stash_signature(&repo)?;
            match repo.stash_save(&sig, &name, Some(git2::StashFlags::INCLUDE_UNTRACKED)) {
                Ok(_) => Some(name),
                Err(e) if Self::is_empty_stash_error(&e) => {
                    eprintln!("   ℹ️ 检测到 dirty 状态但没有可 stash 的内容，继续执行备份同步。");
                    None
                }
                Err(e) if Self::is_symlink_filename_too_long_error(&e) => {
                    Self::stash_with_native_git_no_symlinks(path, &name)?;
                    needs_no_symlink_reset = true;
                    Some(name)
                }
                Err(e) => return Err(e.into()),
            }
        } else {
            if has_unmerged_index {
                eprintln!("   ⚠️ 检测到未合并索引，备份模式将跳过 stash 并硬重置到远程。");
            }
            None
        };

        // 3. 硬重置到远程跟踪分支；这一步负责处理普通落后和分叉历史。
        let reset_result = (|| -> Result<()> {
            let remote_name = Self::get_remote_name(&repo)?.unwrap_or_else(|| "origin".to_string());
            let remote_ref_name = format!("refs/remotes/{}/{}", remote_name, branch_name);

            let remote_ref = repo
                .find_reference(&remote_ref_name)
                .map_err(|_| GetLatestRepoError::RemoteBranchMissing)?;
            let remote_oid = remote_ref
                .target()
                .ok_or_else(|| GetLatestRepoError::RemoteBranchNoTarget)?;

            if needs_no_symlink_reset {
                Self::reset_hard_with_native_git_no_symlinks(path, &remote_ref_name)?;
            } else {
                Self::reset_hard_to_remote(&repo, path, &remote_ref_name, remote_oid)?;
            }
            Self::ensure_not_cancelled(cancellation_checker)?;
            Self::force_update_submodules_for_backup(path, cancellation_checker)?;

            Ok(())
        })();

        match reset_result {
            Ok(()) => {
                // 4. 备份模式不自动恢复 stash。stash 的作用是给用户保留本地改动
                //    的手动恢复点；自动 pop 会把 dirty 状态重新带回工作区，导致
                //    “hard reset 到远程”名不副实。
                if let Some(stash_name) = stash_name {
                    eprintln!("   📦 本地修改已保存到 stash: {}", stash_name);
                }
                Ok((PullForceOutcome::Success, archive_ref))
            }
            Err(e) => {
                if stash_name.is_some() {
                    eprintln!("   ⚠️ 硬重置失败，但本地修改已保存到 stash。请手动恢复。");
                }
                Err(e)
            }
        }
    }

    /// 在备份模式重置父仓库前，先保存所有子模块自己的本地修改。
    ///
    /// Git 子模块是独立仓库：父仓库只能看到 gitlink 是否变化，不能通过父仓库
    /// stash 保存子模块内部文件。这里使用原生 `git submodule foreach`，逐个子模块
    /// 检查 `status --porcelain`，只有确实有变更时才创建包含未跟踪文件的 stash。
    /// 子模块没有变更或仓库没有子模块时命令是空操作，不影响普通仓库。
    fn stash_dirty_submodules_for_backup(
        path: &Path,
        cancellation_checker: Option<&(dyn Fn() -> bool + Send + Sync)>,
    ) -> Result<()> {
        Self::run_native_git_with_cancellation(
            path,
            &[
                "submodule",
                "foreach",
                "--recursive",
                r#"if [ -n "$(git status --porcelain --untracked-files=all)" ]; then git stash push -u -m "getlatestrepo: submodule backup before pull-backup"; fi"#,
            ],
            "保存子模块本地修改",
            cancellation_checker,
        )
        .map(|_| ())
    }

    /// 把子模块强制同步到父仓库当前提交记录的 gitlink。
    ///
    /// `repo.reset(..., Hard)` 只重置父仓库索引和工作区，不会递归进入子模块。
    /// 因此备份模式在父仓库 reset 成功后还要强制 `submodule update`，再对每个
    /// 子模块执行 `reset --hard && clean -fd`，保证父仓库 `status` 不再因为子模块
    /// 内部工作区残留而继续显示 dirty。
    fn force_update_submodules_for_backup(
        path: &Path,
        cancellation_checker: Option<&(dyn Fn() -> bool + Send + Sync)>,
    ) -> Result<()> {
        Self::run_native_git_with_cancellation(
            path,
            &["submodule", "sync", "--recursive"],
            "同步子模块 URL",
            cancellation_checker,
        )?;
        Self::run_native_git_with_cancellation(
            path,
            &["submodule", "update", "--init", "--recursive", "--force"],
            "强制更新子模块",
            cancellation_checker,
        )?;
        Self::run_native_git_with_cancellation(
            path,
            &[
                "submodule",
                "foreach",
                "--recursive",
                "git reset --hard && git clean -fd",
            ],
            "清理子模块工作区",
            cancellation_checker,
        )
        .map(|_| ())
    }

    fn ensure_not_cancelled(
        cancellation_checker: Option<&(dyn Fn() -> bool + Send + Sync)>,
    ) -> Result<()> {
        if crate::signal_handler::is_shutdown_requested()
            || cancellation_checker.is_some_and(|checker| checker())
        {
            return Err(GetLatestRepoError::Other(anyhow::anyhow!("操作已取消")));
        }
        Ok(())
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
        Self::persist_core_symlinks_false(path)?;
        Self::run_native_git(
            path,
            &[
                "-c",
                "core.symlinks=false",
                "reset",
                "--hard",
                remote_ref_name,
            ],
            "禁用 symlink 的原生 git 回退重置",
        )
        .map(|_| ())
    }

    /// 使用原生 git 在禁用 symlink 检出的模式下创建 stash。
    ///
    /// 有些仓库把普通长文本错误提交成 symlink blob。libgit2 的 `stash_save`
    /// 会按 symlink 检出恢复工作区，从而因为 target 过长失败；原生 git 可以通过
    /// `core.symlinks=false` 把这类 symlink blob 写成普通文件，先完成 stash，
    /// 后续 hard reset fallback 也会使用相同策略恢复工作区。
    fn stash_with_native_git_no_symlinks(path: &Path, stash_name: &str) -> Result<()> {
        Self::persist_core_symlinks_false(path)?;
        Self::run_native_git(
            path,
            &[
                "-c",
                "core.symlinks=false",
                "stash",
                "push",
                "-u",
                "-m",
                stash_name,
            ],
            "禁用 symlink 的原生 git stash",
        )
        .map(|_| ())
    }

    /// 持久设置目标仓库 `core.symlinks=false`。
    ///
    /// 这不是全局配置，只写入当前仓库 `.git/config`。对于包含超长 symlink blob 的
    /// 仓库，临时 `git -c core.symlinks=false` 能完成 checkout，但后续 status 仍会按
    /// 默认 symlink 语义判断 typechange；持久配置后，Git/libgit2 才会把普通文件形态
    /// 视为该仓库在当前文件系统上的稳定镜像状态。
    fn persist_core_symlinks_false(path: &Path) -> Result<()> {
        Self::run_native_git(
            path,
            &["config", "core.symlinks", "false"],
            "写入 core.symlinks=false",
        )
        .map(|_| ())
    }

    /// 执行只作用于目标仓库工作区的原生 git 命令。
    ///
    /// 这里集中处理启动失败、退出码、stderr 截断和非交互环境变量，避免多个
    /// fallback 路径各自拼错误信息。调用者传入的命令不得修改远程仓库或本项目
    /// 历史，只用于目标备份仓库的工作区恢复。
    fn run_native_git(path: &Path, args: &[&str], action: &str) -> Result<String> {
        Self::run_native_git_with_cancellation(path, args, action, None)
    }

    fn run_native_git_with_cancellation(
        path: &Path,
        args: &[&str],
        action: &str,
        cancellation_checker: Option<&(dyn Fn() -> bool + Send + Sync)>,
    ) -> Result<String> {
        use std::io::Read;
        use std::process::Stdio;

        let mut command = std::process::Command::new("git");
        command
            .arg("-C")
            .arg(path)
            // 原生 Git 的子模块 stash 同样需要身份；`-c` 仅作用于本次子进程。
            .args(["-c", &format!("user.name={}", Self::AUTOMATION_USER_NAME)])
            .args(["-c", &format!("user.email={}", Self::AUTOMATION_USER_EMAIL)])
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self::configure_process_group(&mut command);
        let mut child = command.spawn().map_err(|error| {
            GetLatestRepoError::Other(anyhow::anyhow!(
                "无法启动原生 git 执行{}: {}",
                action,
                error
            ))
        })?;
        let mut stdout_handle = child.stdout.take().map(|mut output| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = output.read_to_end(&mut bytes);
                bytes
            })
        });
        let mut stderr_handle = child.stderr.take().map(|mut output| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = output.read_to_end(&mut bytes);
                bytes
            })
        });
        let started_at = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(10 * 60);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if crate::signal_handler::is_shutdown_requested()
                        || cancellation_checker.is_some_and(|checker| checker())
                    {
                        Self::terminate_process_group(&mut child);
                        let _ = stdout_handle.take().and_then(|handle| handle.join().ok());
                        let _ = stderr_handle.take().and_then(|handle| handle.join().ok());
                        return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                            "{}已取消",
                            action
                        )));
                    }
                    if started_at.elapsed() >= timeout {
                        Self::terminate_process_group(&mut child);
                        let _ = stdout_handle.take().and_then(|handle| handle.join().ok());
                        let _ = stderr_handle.take().and_then(|handle| handle.join().ok());
                        return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                            "{}超时（600 秒）",
                            action
                        )));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(error) => {
                    Self::terminate_process_group(&mut child);
                    let _ = stdout_handle.take().and_then(|handle| handle.join().ok());
                    let _ = stderr_handle.take().and_then(|handle| handle.join().ok());
                    return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                        "等待{}结束失败: {}",
                        action,
                        error
                    )));
                }
            }
        };
        let stdout_bytes = stdout_handle
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        let stderr_bytes = stderr_handle
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        let stdout = String::from_utf8_lossy(&stdout_bytes);
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        if status.success() {
            return Ok(format!("{}{}", stdout, stderr));
        }
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        Err(GetLatestRepoError::Other(anyhow::anyhow!(
            "{}失败: {}",
            action,
            detail
        )))
    }

    #[cfg(unix)]
    fn configure_process_group(command: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    #[cfg(not(unix))]
    fn configure_process_group(_command: &mut std::process::Command) {}

    /// 先温和终止整个进程组；一秒后仍未退出则强杀，并始终 wait 回收子进程。
    fn terminate_process_group(child: &mut std::process::Child) {
        #[cfg(unix)]
        {
            let process_group = -(child.id() as i32);
            // SAFETY: process_group 由刚创建的子进程 PID 推导，只向该独立进程组发信号。
            unsafe {
                libc::kill(process_group, libc::SIGTERM);
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while std::time::Instant::now() < deadline {
                if child.try_wait().is_ok_and(|status| status.is_some()) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            // SAFETY: 同上；仅清理仍存活的独立子进程组。
            unsafe {
                libc::kill(process_group, libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        let _ = child.kill();
        let _ = child.wait();
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
                .unwrap_or_else(|_| "(no message)".to_string());

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
                .unwrap_or_else(|_| "(no message)".to_string());
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
            if let Ok(path) = entry.path() {
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

        // 父仓库的 checkout/reset 只能恢复 gitlink，不能进入已初始化的子模块。
        // 按父仓库记录的提交逐个恢复现有子模块，不初始化缺失子模块，也不访问网络。
        Self::discard_initialized_submodule_changes(path, include_untracked)?;

        // 丢弃操作必须满足“工作区已干净”的后置条件，禁止返回假成功。
        let (still_dirty, remaining_changes) = Self::check_dirty(&repo)?;
        if still_dirty {
            let mut remaining_paths = remaining_changes
                .iter()
                .take(10)
                .map(|change| change.path.as_str())
                .collect::<Vec<_>>()
                .join("、");
            if remaining_changes.len() > 10 {
                remaining_paths.push_str(&format!(" 等 {} 项", remaining_changes.len()));
            }
            return Err(GetLatestRepoError::Other(anyhow::anyhow!(
                "本地修改未能全部丢弃，仍存在：{}",
                remaining_paths
            )));
        }

        Ok(discarded_files)
    }

    /// 恢复所有已初始化子模块的文件和 gitlink 指向，缺失子模块保持未初始化状态。
    fn discard_initialized_submodule_changes(path: &Path, include_untracked: bool) -> Result<()> {
        let command = if include_untracked {
            r#"git reset --hard "$sha1" && git clean -fd"#
        } else {
            r#"git reset --hard "$sha1""#
        };
        Self::run_native_git(
            path,
            &["submodule", "foreach", "--recursive", command],
            "丢弃子模块本地修改",
        )
        .map(|_| ())
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

    fn run_git_for_test(args: &[&str], cwd: Option<&std::path::Path>) -> String {
        run_git_for_test_with_stdin(args, cwd, None)
    }

    fn run_git_for_test_with_stdin(
        args: &[&str],
        cwd: Option<&std::path::Path>,
        stdin: Option<&[u8]>,
    ) -> String {
        let mut command = std::process::Command::new("git");
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }

        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if stdin.is_some() {
            command.stdin(std::process::Stdio::piped());
        }
        let mut child = command.spawn().expect("spawn git command");
        if let Some(input) = stdin {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .expect("git stdin should be piped")
                .write_all(input)
                .expect("write git stdin");
        }
        let output = child.wait_with_output().expect("wait git command");
        assert!(
            output.status.success(),
            "git {:?} failed\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn untracked_finder_metadata_does_not_mark_repository_dirty() {
        let (_tmp, path, _) = create_repo_with_tracking();
        std::fs::write(path.join(".DS_Store"), "finder metadata").unwrap();

        let repo = git2::Repository::open(&path).unwrap();
        let (dirty, changes) = GitOps::check_dirty(&repo).unwrap();

        assert!(!dirty);
        assert!(changes.is_empty());
    }

    #[test]
    fn directory_containing_only_finder_metadata_is_not_dirty() {
        let (_tmp, path, _) = create_repo_with_tracking();
        std::fs::create_dir(path.join("migration")).unwrap();
        std::fs::write(path.join("migration/.DS_Store"), "finder metadata").unwrap();

        let repo = git2::Repository::open(&path).unwrap();
        let (dirty, changes) = GitOps::check_dirty(&repo).unwrap();

        assert!(!dirty);
        assert!(changes.is_empty());
    }

    #[test]
    fn untracked_directory_with_real_file_remains_dirty() {
        let (_tmp, path, _) = create_repo_with_tracking();
        std::fs::create_dir(path.join("migration")).unwrap();
        std::fs::write(path.join("migration/.DS_Store"), "finder metadata").unwrap();
        std::fs::write(path.join("migration/data.sql"), "real data").unwrap();

        let repo = git2::Repository::open(&path).unwrap();
        let (dirty, changes) = GitOps::check_dirty(&repo).unwrap();

        assert!(dirty);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "migration/");
    }

    #[test]
    fn tracked_finder_metadata_changes_are_still_reported() {
        let (_tmp, path, _) = create_repo_with_tracking();
        std::fs::write(path.join(".DS_Store"), "tracked metadata").unwrap();
        run_git_for_test(&["add", "--force", ".DS_Store"], Some(&path));
        run_git_for_test(&["commit", "-m", "track finder metadata"], Some(&path));
        std::fs::write(path.join(".DS_Store"), "changed metadata").unwrap();

        let repo = git2::Repository::open(&path).unwrap();
        let (dirty, changes) = GitOps::check_dirty(&repo).unwrap();

        assert!(dirty);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, ".DS_Store");
    }

    #[test]
    fn hydrated_lfs_content_matching_pointer_is_not_dirty() {
        let (_tmp, path, _) = create_repo_with_tracking();
        let content = b"hydrated lfs object\n";
        let oid = GitOps::hex_bytes(Sha256::digest(content).as_ref());
        let pointer = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
            content.len()
        );
        std::fs::write(path.join("asset.bin"), pointer).unwrap();
        run_git_for_test(&["add", "asset.bin"], Some(&path));
        run_git_for_test(&["commit", "-m", "track lfs pointer"], Some(&path));
        std::fs::write(path.join("asset.bin"), content).unwrap();

        let repo = git2::Repository::open(&path).unwrap();
        let (dirty, changes) = GitOps::check_dirty(&repo).unwrap();

        assert!(!dirty);
        assert!(changes.is_empty());
    }

    #[test]
    fn hydrated_lfs_content_with_different_hash_remains_dirty() {
        let (_tmp, path, _) = create_repo_with_tracking();
        let content = b"hydrated lfs object\n";
        let oid = GitOps::hex_bytes(Sha256::digest(content).as_ref());
        let pointer = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
            content.len()
        );
        std::fs::write(path.join("asset.bin"), pointer).unwrap();
        run_git_for_test(&["add", "asset.bin"], Some(&path));
        run_git_for_test(&["commit", "-m", "track lfs pointer"], Some(&path));
        std::fs::write(path.join("asset.bin"), b"modified lfs object\n").unwrap();

        let repo = git2::Repository::open(&path).unwrap();
        let (dirty, changes) = GitOps::check_dirty(&repo).unwrap();

        assert!(dirty);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "asset.bin");
    }

    #[test]
    fn post_fetch_refresh_reuses_worktree_state_and_updates_remote_distance() {
        let (_tmp, path, first_commit) = create_repo_with_tracking();
        let repo = git2::Repository::open(&path).unwrap();
        let signature = git2::Signature::now("remote", "remote@example.com").unwrap();
        let parent = repo.find_commit(first_commit).unwrap();
        let tree = parent.tree().unwrap();
        let remote_commit = repo
            .commit(None, &signature, &signature, "remote", &tree, &[&parent])
            .unwrap();
        repo.reference(
            "refs/remotes/origin/main",
            remote_commit,
            true,
            "模拟 fetch",
        )
        .unwrap();
        repo.remote("origin", "https://example.com/repository.git")
            .unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("branch.main.remote", "origin").unwrap();
        config
            .set_str("branch.main.merge", "refs/heads/main")
            .unwrap();
        let mut cached = GitOps::inspect(&path, path.to_str().unwrap()).unwrap();
        cached.dirty = true;
        cached.dirty_files = vec!["保留的工作区状态.txt".to_string()];

        let refreshed = GitOps::refresh_remote_state_after_fetch(&cached).unwrap();

        assert!(refreshed.dirty);
        assert_eq!(
            refreshed.dirty_files,
            vec!["保留的工作区状态.txt".to_string()]
        );
        assert_eq!(refreshed.behind_count, 1);
        assert_eq!(refreshed.freshness, Freshness::HasUpdates);
    }

    #[test]
    fn stash_signature_falls_back_without_git_identity() {
        let (_tmp, path, _) = create_repo_with_tracking();
        let repo = git2::Repository::open(&path).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "").unwrap();
        config.set_str("user.email", "").unwrap();

        let signature = GitOps::stash_signature(&repo).unwrap();

        assert_eq!(signature.name(), Ok(GitOps::AUTOMATION_USER_NAME));
        assert_eq!(signature.email(), Ok(GitOps::AUTOMATION_USER_EMAIL));
    }

    #[cfg(unix)]
    #[test]
    fn managed_native_git_cancellation_reaps_process_group() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let directory = tempfile::tempdir().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation_signal = Arc::clone(&cancelled);
        let setter = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            cancellation_signal.store(true, Ordering::Release);
        });
        let checker = || cancelled.load(Ordering::Acquire);
        let started_at = std::time::Instant::now();

        let result = GitOps::run_native_git_with_cancellation(
            directory.path(),
            &["-c", "alias.glr-wait=!sleep 30", "glr-wait"],
            "取消测试",
            Some(&checker),
        );
        setter.join().unwrap();

        assert!(result.is_err());
        assert!(started_at.elapsed() < std::time::Duration::from_secs(3));
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
    fn test_pull_ff_only_rejects_new_local_changes_without_data_loss() {
        let (_tmp, path, c1) = create_repo_with_tracking();
        let c2 = add_commit_from_parent(&path, c1, "remote update");
        move_tracking_ref(&path, c2);

        // 模拟扫描完成后、实际 Pull 前才出现的本地修改。
        let local_file = path.join("local-only.txt");
        std::fs::write(&local_file, "必须保留的本地内容").unwrap();

        let result = GitOps::pull_ff_only(&path);

        assert!(matches!(result, Err(GetLatestRepoError::DirtyWorkingTree)));
        assert_eq!(
            std::fs::read_to_string(&local_file).unwrap(),
            "必须保留的本地内容"
        );
        let repo = git2::Repository::open(&path).unwrap();
        assert_eq!(repo.head().unwrap().target(), Some(c1));
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

    /// 验证远程新增文件与本地未跟踪文件冲突时，安全拉取拒绝覆盖本地数据。
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

        // `pull-safe` 必须在更新引用前拒绝操作，不能把未跟踪文件当成可清理内容。
        let result = GitOps::pull_ff_only(&path);
        assert!(matches!(result, Err(GetLatestRepoError::DirtyWorkingTree)));

        // 验证工作区内容与本地分支引用均保持不变。
        let repo_after = git2::Repository::open(&path).unwrap();
        let content = std::fs::read_to_string(&new_file_path).unwrap();
        assert_eq!(content, "untracked local content");
        assert_eq!(repo_after.head().unwrap().target(), Some(c1));
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
                if let Ok(name) = r.name() {
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
                if let (Ok(name), Some(oid)) = (r.name(), r.target()) {
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
    fn test_pull_backup_cleans_dirty_up_to_date_repo() {
        let (_tmp, path, c1) = create_repo_with_tracking();
        let repo = git2::Repository::open(&path).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        std::fs::write(path.join("tracked.txt"), "remote tracked\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.find_commit(c1).unwrap();
        let c2 = repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                "add tracked file",
                &tree,
                &[&parent],
            )
            .unwrap();
        move_tracking_ref(&path, c2);

        std::fs::write(path.join("tracked.txt"), "local dirty change\n").unwrap();
        std::fs::write(path.join("untracked.txt"), "local untracked\n").unwrap();

        let result = GitOps::pull_backup(&path);
        assert!(
            result.is_ok(),
            "Expected dirty up-to-date backup repo to reset successfully, got: {:?}",
            result
        );

        let repo = git2::Repository::open(&path).unwrap();
        let (is_dirty, dirty_files) = GitOps::check_dirty(&repo).unwrap();
        assert!(
            !is_dirty,
            "pull-backup should clean dirty files, remaining: {:?}",
            dirty_files
        );
        assert_eq!(
            std::fs::read_to_string(path.join("tracked.txt")).unwrap(),
            "remote tracked\n"
        );
        assert!(
            !path.join("untracked.txt").exists(),
            "untracked backup-only file should be removed after hard reset"
        );
    }

    #[test]
    fn test_pull_backup_cleans_dirty_submodule() {
        let (_tmp, path, c1) = create_repo_with_tracking();
        let submodule_tmp = TempDir::new().unwrap();
        let submodule_path = submodule_tmp.path();

        run_git_for_test(&["init"], Some(submodule_path));
        run_git_for_test(&["config", "user.name", "test"], Some(submodule_path));
        run_git_for_test(
            &["config", "user.email", "test@test.com"],
            Some(submodule_path),
        );
        std::fs::write(submodule_path.join("nested.txt"), "remote nested\n").unwrap();
        run_git_for_test(&["add", "nested.txt"], Some(submodule_path));
        run_git_for_test(&["commit", "-m", "submodule c1"], Some(submodule_path));

        run_git_for_test(
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                submodule_path.to_str().unwrap(),
                "deps/sub",
            ],
            Some(&path),
        );

        let repo = git2::Repository::open(&path).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new(".gitmodules")).unwrap();
        index.add_path(std::path::Path::new("deps/sub")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.find_commit(c1).unwrap();
        let c2 = repo
            .commit(Some("HEAD"), &sig, &sig, "add submodule", &tree, &[&parent])
            .unwrap();
        move_tracking_ref(&path, c2);
        drop(parent);
        drop(tree);
        drop(repo);

        let checked_out_submodule = path.join("deps/sub");
        std::fs::write(
            checked_out_submodule.join("nested.txt"),
            "local dirty nested\n",
        )
        .unwrap();

        let result = GitOps::pull_backup(&path);
        assert!(
            result.is_ok(),
            "Expected dirty submodule backup repo to reset successfully, got: {:?}",
            result
        );

        let repo = git2::Repository::open(&path).unwrap();
        let (is_dirty, dirty_files) = GitOps::check_dirty(&repo).unwrap();
        assert!(
            !is_dirty,
            "pull-backup should clean dirty submodule status, remaining: {:?}",
            dirty_files
        );
        assert_eq!(
            std::fs::read_to_string(checked_out_submodule.join("nested.txt")).unwrap(),
            "remote nested\n"
        );
    }

    #[test]
    fn test_discard_changes_restores_initialized_submodule_to_recorded_commit() {
        let (_tmp, path, c1) = create_repo_with_tracking();
        let submodule_tmp = TempDir::new().unwrap();
        let submodule_path = submodule_tmp.path();

        run_git_for_test(&["init"], Some(submodule_path));
        run_git_for_test(&["config", "user.name", "test"], Some(submodule_path));
        run_git_for_test(
            &["config", "user.email", "test@test.com"],
            Some(submodule_path),
        );
        std::fs::write(submodule_path.join("nested.txt"), "recorded\n").unwrap();
        run_git_for_test(&["add", "nested.txt"], Some(submodule_path));
        run_git_for_test(
            &["commit", "-m", "submodule recorded"],
            Some(submodule_path),
        );

        run_git_for_test(
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                submodule_path.to_str().unwrap(),
                "deps/sub",
            ],
            Some(&path),
        );
        let repo = git2::Repository::open(&path).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new(".gitmodules")).unwrap();
        index.add_path(std::path::Path::new("deps/sub")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.find_commit(c1).unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "record submodule",
            &tree,
            &[&parent],
        )
        .unwrap();
        drop(parent);
        drop(tree);
        drop(repo);

        let checked_out_submodule = path.join("deps/sub");
        std::fs::write(checked_out_submodule.join("nested.txt"), "local change\n").unwrap();
        std::fs::write(checked_out_submodule.join("untracked.txt"), "remove me\n").unwrap();

        let discarded = GitOps::discard_changes(&path, true).unwrap();

        assert!(discarded.iter().any(|file| file == "deps/sub"));
        let repo = git2::Repository::open(&path).unwrap();
        let (dirty, remaining) = GitOps::check_dirty(&repo).unwrap();
        assert!(!dirty, "丢弃后仍有本地修改：{remaining:?}");
        assert_eq!(
            std::fs::read_to_string(checked_out_submodule.join("nested.txt")).unwrap(),
            "recorded\n"
        );
        assert!(!checked_out_submodule.join("untracked.txt").exists());
    }

    #[test]
    fn test_pull_backup_handles_deleted_oversized_symlink_blob() {
        let (_tmp, path, c1) = create_repo_with_tracking();
        let long_target = "a".repeat(4096);
        let blob_id = run_git_for_test_with_stdin(
            &["hash-object", "-w", "--stdin"],
            Some(&path),
            Some(long_target.as_bytes()),
        );

        run_git_for_test(
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("120000,{blob_id},skills/last30days/SKILL.md"),
            ],
            Some(&path),
        );
        let c2 = add_commit(&path, "add oversized symlink blob");
        move_tracking_ref(&path, c2);

        run_git_for_test(
            &["-c", "core.symlinks=false", "reset", "--hard", "HEAD"],
            Some(&path),
        );
        std::fs::remove_file(path.join("skills/last30days/SKILL.md")).unwrap();

        let result = GitOps::pull_backup(&path);
        assert!(
            result.is_ok(),
            "Expected oversized symlink fallback to reset successfully, got: {:?}",
            result
        );

        let repo = git2::Repository::open(&path).unwrap();
        let (is_dirty, dirty_files) = GitOps::check_dirty(&repo).unwrap();
        assert!(
            !is_dirty,
            "oversized symlink fallback should leave repo clean, remaining: {:?}",
            dirty_files
        );
        assert_eq!(
            std::fs::read_to_string(path.join("skills/last30days/SKILL.md")).unwrap(),
            long_target
        );
        let head_oid = repo.head().unwrap().target().unwrap();
        assert_eq!(head_oid, c2);
        assert_ne!(head_oid, c1);
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

    #[test]
    fn test_classify_http_403_before_generic_curl_message() {
        let status = GitOps::classify_error(
            "fatal: unable to access 'https://example.com/private.git/': The requested URL returned error: 403",
        );

        assert!(matches!(status, FetchStatus::AuthenticationRequired { .. }));
    }

    #[test]
    fn test_classify_http_404_before_generic_curl_message() {
        let status = GitOps::classify_error(
            "fatal: unable to access 'https://example.com/missing.git/': The requested URL returned error: 404",
        );

        assert!(matches!(status, FetchStatus::RepositoryNotFound { .. }));
    }

    #[test]
    fn test_classify_rate_limited_403_as_network_error() {
        let status = GitOps::classify_error(
            "fatal: unable to access URL: HTTP 403 secondary rate limit exceeded",
        );

        assert!(matches!(status, FetchStatus::NetworkError { .. }));
    }

    #[test]
    fn fetch_proxy_overrides_repository_local_proxy() {
        let git_ops = GitOps::with_proxy(ProxyConfig {
            enabled: true,
            http_proxy: "http://host.docker.internal:7890".to_string(),
            https_proxy: "http://host.docker.internal:7890".to_string(),
        });
        let mut command = std::process::Command::new("git");

        git_ops.configure_fetch_proxy(&mut command, "origin");

        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(arguments.contains(&"http.proxy=http://host.docker.internal:7890".to_string()));
        assert!(arguments.contains(&"https.proxy=http://host.docker.internal:7890".to_string()));
        assert!(
            arguments.contains(&"remote.origin.proxy=http://host.docker.internal:7890".to_string())
        );
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == "HTTPS_PROXY")
                .and_then(|(_, value)| value)
                .map(|value| value.to_string_lossy().into_owned()),
            Some("http://host.docker.internal:7890".to_string())
        );
    }
}
