//! 信号处理模块
//!
//! 提供三层优雅关闭策略：
//! 1. 在长循环中密集检查关闭标志（fetcher、executor、concurrent）
//! 2. main 函数末尾直接执行 `process::exit()`，避免 tokio runtime 因本后台任务而无法退出
//! 3. 10 秒强制退出兜底 + 第二次 Ctrl+C 立即退出
//!
//! 保证：用户在对 1000 个仓库执行 fetch 时按 Ctrl+C 也不会卡住。
//!
//! ⚠️ 注意：本模块通过 `tokio::spawn` 启动的后台任务会无限期等待 `ctrl_c()`。
//! 若 main 正常返回 `ExitCode` 而不调用 `process::exit()`，tokio runtime 将进入 shutdown
//! 并等待本任务完成，导致进程在程序结束后无法退出（此时再按 Ctrl+C 也可能因 runtime
//! 处于关闭状态而无法被正确处理）。因此 `main.rs` 末尾必须直接 `process::exit()`。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// shell 对 Ctrl+C 的标准退出码，便于脚本准确识别任务未完成。
pub const INTERRUPTED_EXIT_CODE: i32 = 130;

/// 全局关闭标志
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 防止重复初始化
static INIT_CALLED: AtomicBool = AtomicBool::new(false);

/// 初始化信号处理
///
/// 第一次 Ctrl+C → 设置关闭标志，启动 10 秒强制退出定时器
/// 第二次 Ctrl+C → 立即 `process::exit(130)`
/// 10 秒超时   → `process::exit(130)`
///
/// 本函数是幂等的：多次调用不会产生额外的后台任务。
pub fn init() {
    if INIT_CALLED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    tokio::spawn(async {
        // 等待第一次 Ctrl+C
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }

        eprintln!("\n⚠️  收到中断信号，正在优雅关闭...");
        eprintln!("    再按一次 Ctrl+C 可立即强制退出");
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);

        // 10 秒超时与第二次 Ctrl+C 竞争
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {
                eprintln!("\n⚠️  优雅关闭超时（10 秒），正在强制退出");
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\n✗ 收到第二次中断信号，立即强制退出");
            }
        }

        std::process::exit(INTERRUPTED_EXIT_CODE);
    });
}

/// 检查是否收到关闭请求
pub fn is_shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn test_shutdown_flag_logic() {
        // Test AtomicBool logic with a local instance to avoid global state race
        let flag = AtomicBool::new(false);
        assert!(!flag.load(Ordering::Relaxed));
        flag.store(true, Ordering::SeqCst);
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn interrupted_exit_code_is_not_success() {
        assert_eq!(INTERRUPTED_EXIT_CODE, 130);
        assert_ne!(INTERRUPTED_EXIT_CODE, 0);
    }
}
