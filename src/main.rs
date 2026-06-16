mod cli;
mod commands;
mod concurrent;
mod config;
mod db;
mod error;
mod fetcher;
mod git;
mod models;
mod reporter;
mod scanner;
mod security;
mod signal_handler;
mod sync;
mod utils;
mod workflow;

use crate::cli::{Cli, Commands};
use crate::config::AppConfig;
use crate::db::Database;
use crate::git::ProxyConfig;
use anyhow::{Context, Result};
use clap::Parser;
use std::fs::File;

/// Process lock file; automatically cleaned up on Drop
pub struct ProcessLock {
    #[cfg(unix)]
    _file: File,
    #[cfg(not(unix))]
    pid_path: std::path::PathBuf,
}

#[cfg(not(unix))]
impl Drop for ProcessLock {
    fn drop(&mut self) {
        // Windows: 仅当 PID 文件内容匹配当前进程时才删除，避免竞态下误删其他实例的锁
        if let Ok(content) = std::fs::read_to_string(&self.pid_path) {
            if let Ok(pid) = content.trim().parse::<u32>() {
                if pid == std::process::id() {
                    if let Err(e) = std::fs::remove_file(&self.pid_path) {
                        eprintln!(
                            "警告：无法清理 PID 文件 '{}': {}",
                            self.pid_path.display(),
                            e
                        );
                    }
                }
            }
        }
    }
}

/// Acquire a process lock to prevent duplicate execution
fn acquire_process_lock() -> Result<ProcessLock> {
    let lock_path = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("getlatestrepo.lock");

    // cache 目录在精简系统、CI 或 HOME 被重定向时可能不存在。
    // 先创建父目录，避免所有命令在真正解析 CLI 前就因为锁文件路径失败。
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("无法创建锁文件目录: {:?}", parent))?;
    }

    #[cfg(unix)]
    {
        use libc::{LOCK_EX, LOCK_NB, flock};
        use std::os::unix::io::AsRawFd;

        let file =
            File::create(&lock_path).with_context(|| format!("无法创建锁文件: {:?}", lock_path))?;

        let fd = file.as_raw_fd();
        // SAFETY: `fd` is a valid file descriptor obtained from `File::create` above.
        // `flock` is a POSIX system call that operates on file descriptors atomically.
        // `LOCK_EX | LOCK_NB` requests a non-blocking exclusive lock; if the lock is already
        // held by another process, it returns -1 with errno=EWOULDBLOCK, which we handle below.
        // No undefined behavior can occur from this call.
        let result = unsafe { flock(fd, LOCK_EX | LOCK_NB) };

        if result != 0 {
            anyhow::bail!("另一个 getlatestrepo 实例正在运行，无法并发执行");
        }

        Ok(ProcessLock { _file: file })
    }

    #[cfg(not(unix))]
    {
        // Windows: atomic PID file creation with stale-lock recovery
        let pid_file = lock_path.with_extension("pid");

        // Use OpenOptions with create_new for atomic creation (no TOCTOU race)
        let current_pid = std::process::id();
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pid_file)
        {
            Ok(mut f) => {
                use std::io::Write;
                let _ = write!(f, "{}", current_pid);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Lock file exists — check if the owning process is still alive
                let mut acquired = false;
                for attempt in 0..3 {
                    if let Ok(pid_str) = std::fs::read_to_string(&pid_file) {
                        if let Ok(pid) = pid_str.trim().parse::<u32>() {
                            if is_process_running(pid) {
                                anyhow::bail!("另一个 getlatestrepo 实例正在运行（PID: {}）", pid);
                            }
                        }
                    }
                    // Stale lock — remove and retry atomically
                    let _ = std::fs::remove_file(&pid_file);
                    match std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&pid_file)
                    {
                        Ok(mut f) => {
                            use std::io::Write;
                            let _ = write!(f, "{}", current_pid);
                            acquired = true;
                            break;
                        }
                        Err(_) if attempt < 2 => {
                            // 可能被其他进程抢先，短暂等待后重试
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                        Err(e) => {
                            anyhow::bail!("删除过期 PID 文件后仍无法获取锁: {}", e);
                        }
                    }
                }
                if !acquired {
                    anyhow::bail!("无法获取锁：多次重试后仍无法替换过期 PID 文件");
                }
            }
            Err(e) => {
                anyhow::bail!("无法创建 PID 文件: {}", e);
            }
        }

        Ok(ProcessLock { pid_path: pid_file })
    }
}

#[cfg(not(unix))]
fn is_process_running(pid: u32) -> bool {
    use std::process::Command;

    // Use tasklist to check if process exists and is getlatestrepo
    // /FO CSV produces machine-parseable output: "Image Name","PID",...
    let output = Command::new("tasklist")
        .args(&["/FI", &format!("PID eq {}", pid), "/NH", "/FO", "CSV"])
        .output();

    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // tasklist returns "INFO: No tasks are running" when PID not found
            if stdout.contains("No tasks") || stdout.trim().is_empty() {
                return false;
            }
            // 验证进程名包含 getlatestrepo，降低 PID 复用误报概率
            stdout.to_ascii_lowercase().contains("getlatestrepo")
        }
        Err(_) => {
            // If unable to check, assume process exists (conservative strategy)
            true
        }
    }
}

#[tokio::main]
async fn main() -> Result<std::process::ExitCode> {
    // Initialize colored output
    colored::control::set_override(true);

    // Initialize signal handling for Ctrl+C
    signal_handler::init();

    // 先解析 CLI，再获取进程锁。`--help` 和 `--version` 属于 clap 的早退出路径，
    // 不应因为缓存目录不可写、已有实例运行或启动自检失败而无法显示基础信息。
    let cli = Cli::parse();
    let no_security_check = cli.no_security_check;
    let auto_skip_high_risk = cli.auto_skip_high_risk;

    // 真正会读写配置、数据库或仓库状态的命令才需要进程锁，防止多个实例并发
    // fetch / scan / pull 时互相覆盖数据库记录或工作区状态。
    let _lock = acquire_process_lock()?;

    // 启动自检：修复路径不一致的记录，清理过期的临时文件。日志中带上
    // 编译期版本号，方便用户确认当前 alias 或安装路径是否已经切到新二进制。
    if !matches!(cli.command, Commands::Init { .. })
        && let Err(e) = run_startup_cleanup()
    {
        eprintln!("⚠️  启动自检失败（v{}）: {e}", app_version());
    }

    // Build proxy config
    let proxy_config = build_proxy_config(cli.proxy, cli.proxy_url);

    let exit_code = match cli.command {
        Commands::Init { path } => commands::init::execute(path).await.map(|_| 0),
        Commands::Scan {
            fetch,
            output,
            out,
            depth,
            jobs,
        } => commands::scan::execute(
            fetch,
            output,
            out,
            depth,
            validate_jobs(jobs),
            no_security_check,
            auto_skip_high_risk,
        )
        .await
        .map(|_| 0),
        Commands::Fetch { jobs, timeout } => commands::fetch::execute(
            validate_jobs(jobs),
            timeout,
            no_security_check,
            auto_skip_high_risk,
            proxy_config,
        )
        .await
        .map(|_| 0),
        Commands::Status { path, diff, issues } => commands::status::execute(path, diff, issues)
            .await
            .map(|_| 0),
        Commands::Config { command } => commands::config::execute(command).await.map(|_| 0),
        Commands::Workflow {
            name,
            list,
            dry_run,
            silent,
            jobs,
            timeout,
            diff_after,
            yes,
            no_pull_guard,
        } => {
            commands::workflow::execute(
                name,
                list,
                dry_run,
                silent,
                jobs.map(validate_jobs),
                timeout,
                diff_after,
                yes,
                no_security_check,
                auto_skip_high_risk,
                no_pull_guard,
                proxy_config,
            )
            .await
        }
        Commands::Discard { path, yes } => commands::discard::execute(path, yes).await.map(|_| 0),
    }?;

    // 若收到关闭请求，立即退出，不等待 tokio runtime 清理后台线程
    if signal_handler::is_shutdown_requested() {
        eprintln!("⚠️  因中断信号提前退出");
        std::process::exit(0);
    }

    // 直接退出，避免 tokio runtime 因后台信号监听任务而无法结束
    std::process::exit(exit_code)
}

/// 启动自检：修复路径已不存在的数据库记录，
/// 并清理残留的 `.getlatestrepo_swap` 临时目录。
fn run_startup_cleanup() -> Result<usize> {
    let config = AppConfig::load()?;
    if !config.is_initialized() {
        return Ok(0);
    }

    let db = Database::open()?;
    let repos = db.list_repositories()?;
    let mut fixes = 0;

    for repo in &repos {
        if std::path::Path::new(&repo.path).exists() {
            continue;
        }

        // 尝试在 needauth/ 目录下定位仓库（保留原始相对路径结构）
        let needauth_root = std::path::Path::new(&repo.root_path).join(crate::utils::NEEDAUTH_DIR);
        let original_relative = std::path::Path::new(&repo.path)
            .strip_prefix(&repo.root_path)
            .unwrap_or(std::path::Path::new(&repo.name));
        let needauth_path = needauth_root.join(original_relative);

        if needauth_path.exists() {
            let mut updated = repo.clone();
            updated.path = needauth_path.to_string_lossy().to_string();
            updated.root_path = needauth_root.to_string_lossy().to_string();
            db.upsert_repository(&mut updated)?;
            fixes += 1;
        } else {
            db.delete_repository(&repo.path)?;
            fixes += 1;
        }
    }

    if fixes > 0 {
        eprintln!(
            "ℹ️  启动自检完成（v{}），已修复 {fixes} 条记录",
            app_version()
        );
    }

    // 清理 needauth/ 下过期的 swap 临时目录
    let mut swap_cleaned = 0;
    for source in &config.scan_sources {
        let needauth = std::path::Path::new(&source.root_path).join(crate::utils::NEEDAUTH_DIR);
        if !needauth.exists() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&needauth) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains(".getlatestrepo_swap") {
                    if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                        eprintln!("⚠️  清理临时目录失败 '{}': {e}", entry.path().display());
                    } else {
                        swap_cleaned += 1;
                    }
                }
            }
        }
    }
    if swap_cleaned > 0 {
        eprintln!("🧹 已清理 {swap_cleaned} 个残留临时目录");
    }

    Ok(fixes)
}

/// 返回当前二进制的编译期版本号。
///
/// `env!("CARGO_PKG_VERSION")` 来自 Cargo 包元数据，能保证 `--version`、启动自检
/// 日志和 release tag 使用同一份版本来源，避免 alias 指向旧二进制时用户无法察觉。
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Build proxy configuration
/// Validate and limit concurrency
fn validate_jobs(jobs: usize) -> usize {
    const MAX_JOBS: usize = 100;
    const MIN_JOBS: usize = 1;

    if jobs > MAX_JOBS {
        eprintln!(
            "警告：并发数 {} 超过最大限制 {}，已调整为 {}",
            jobs, MAX_JOBS, MAX_JOBS
        );
        MAX_JOBS
    } else if jobs < MIN_JOBS {
        eprintln!(
            "警告：并发数 {} 低于最小限制 {}，已调整为 {}",
            jobs, MIN_JOBS, MIN_JOBS
        );
        MIN_JOBS
    } else {
        jobs
    }
}

fn build_proxy_config(proxy: bool, proxy_url: Option<String>) -> Option<ProxyConfig> {
    let has_explicit_url = proxy_url.as_ref().map(|s| !s.is_empty()).unwrap_or(false);
    if proxy || has_explicit_url {
        // --proxy-url applies to both HTTP and HTTPS (user explicitly provided a proxy)
        let default_url = proxy_url
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("HTTP_PROXY").ok())
            .or_else(|| std::env::var("http_proxy").ok())
            .unwrap_or_else(|| crate::utils::DEFAULT_PROXY_URL.to_string());
        let https_url = proxy_url
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("HTTPS_PROXY").ok())
            .or_else(|| std::env::var("https_proxy").ok())
            .unwrap_or_else(|| default_url.clone());
        Some(ProxyConfig {
            enabled: true,
            http_proxy: default_url,
            https_proxy: https_url,
        })
    } else {
        None
    }
}
