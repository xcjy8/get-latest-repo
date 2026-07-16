//! Concurrent execution utility module
//!
//! Provides safe concurrent execution, solving the following problems:
//! - Deadlock risk (proper cleanup when worker thread panics)
//! - Busy-wait issue (uses condition variables)
//! - Error handling (does not silently ignore errors)
//! - Reasonable timeout handling

use std::collections::VecDeque;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;

/// Task results
#[derive(Debug)]
pub struct TaskResult<T> {
    pub index: usize,
    pub result: T,
}

/// Execute multiple tasks concurrently (return raw results)
///
/// # Parameters
/// - `tasks`: Task list, each task is a closure
/// - `max_concurrent`: Maximum concurrency
///
/// # Returns
/// Return result list in original order (panicked tasks return None)
///
/// # Features
/// - Auto-handle panics (returns None)
/// - Use blocking wait (non busy-wait)
/// - 返回前等待全部已启动任务结束，禁止破坏性 Git 操作遗留到后台继续执行
pub fn execute_concurrent_raw<F, T>(tasks: Vec<F>, max_concurrent: usize) -> Vec<Option<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let total = tasks.len();
    if total == 0 {
        return Vec::new();
    }

    let (tx, rx) = channel::<TaskResult<Option<T>>>();
    // 将任务放入共享队列，由固定数量 worker 拉取执行。
    // 这样整体超时从 worker 启动后立即生效；即使前几个 Git/FS 任务永久卡住，
    // 主线程也不会阻塞在“等待空闲槽位”的派发阶段。
    let max_workers = max_concurrent.clamp(1, total);
    let task_queue = Arc::new(Mutex::new(
        tasks
            .into_iter()
            .enumerate()
            .collect::<VecDeque<(usize, F)>>(),
    ));
    let mut handles = Vec::new();

    for worker_index in 0..max_workers {
        let tx_inner = Sender::clone(&tx);
        let task_queue = Arc::clone(&task_queue);

        // 每个 worker 使用较小栈，避免超时后分离的卡住线程浪费过多内存。
        match thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(move || {
                loop {
                    if crate::signal_handler::is_shutdown_requested() {
                        break;
                    }

                    let next_task = match task_queue.lock() {
                        Ok(mut queue) => queue.pop_front(),
                        Err(_) => None,
                    };

                    let Some((index, task)) = next_task else {
                        break;
                    };

                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task));
                    let result = match result {
                        Ok(r) => Some(r),
                        Err(_) => {
                            eprintln!("警告：任务 {} panic", index);
                            None
                        }
                    };

                    let _ = tx_inner.send(TaskResult { index, result });
                }
            }) {
            Ok(handle) => handles.push(handle),
            Err(e) => {
                eprintln!("警告：创建 worker {} 失败: {}", worker_index, e);
            }
        }
    }

    // Close sender
    drop(tx);

    // Collect results
    let mut results: Vec<Option<Option<T>>> = (0..total).map(|_| None).collect();
    let mut received = 0;

    while received < total {
        match rx.recv_timeout(std::time::Duration::from_secs(
            crate::utils::CONCURRENT_RECV_TIMEOUT_SECS,
        )) {
            Ok(task_result) => {
                results[task_result.index] = Some(task_result.result);
                received += 1;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Check if all threads have finished
                let active_handles = handles.iter().filter(|h| !h.is_finished()).count();
                if active_handles == 0 {
                    eprintln!(
                        "警告：{} 个任务未完成，可能已 panic 或发送结果失败",
                        total - received
                    );
                    break;
                }
                // 只报告慢任务，不提前返回。spawn_blocking/thread 无法被安全强杀；
                // 对 Pull/reset 等破坏性操作，等待收尾比制造后台并发修改更安全。
                eprintln!(
                    "警告：仍有 {} 个任务运行超过 {} 秒，继续等待安全收尾",
                    active_handles,
                    crate::utils::CONCURRENT_RECV_TIMEOUT_SECS
                );
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }

    // 所有 worker 都必须 join；函数返回即代表不会再有后台任务访问仓库或队列。
    for handle in handles {
        let _ = handle.join();
    }

    // Flatten results: Option<Option<T>> -> Option<T>
    results.into_iter().map(|r| r.flatten()).collect()
}

/// Execute single task and catch panic
#[allow(dead_code)]
pub fn run_with_catch<F, T>(f: F) -> Result<T, String>
where
    F: FnOnce() -> T + Send + 'static,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => Ok(result),
        Err(_) => Err("任务 panic".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_concurrent_execute() {
        let tasks: Vec<_> = (0..10).map(|i| move || -> i32 { i * 2 }).collect();

        let results = execute_concurrent_raw(tasks, 3);

        assert_eq!(results.len(), 10);
        for (i, result) in results.iter().enumerate() {
            assert_eq!(*result, Some((i * 2) as i32));
        }
    }

    #[test]
    fn test_empty_tasks() {
        let tasks: Vec<Box<dyn FnOnce() -> i32 + Send>> = Vec::new();
        let results = execute_concurrent_raw(tasks, 3);
        assert!(results.is_empty());
    }

    #[test]
    fn test_panic_recovery() {
        let tasks: Vec<Box<dyn FnOnce() -> i32 + Send>> = vec![
            Box::new(|| 1),
            Box::new(|| -> i32 { panic!("task 2 panic") }),
            Box::new(|| 3),
        ];

        let results = execute_concurrent_raw(tasks, 2);

        assert_eq!(results.len(), 3);
        assert_eq!(results[0], Some(1));
        assert_eq!(results[1], None); // panic returns None
        assert_eq!(results[2], Some(3));
    }

    #[test]
    fn test_counter_no_leak_on_panic() {
        // Test that counter doesn't leak even if all tasks panic
        #[allow(clippy::unused_unit)]
        let tasks: Vec<Box<dyn FnOnce() + Send>> = (0..5)
            .map(|i| {
                Box::new(move || -> () { panic!("task {i} panic") }) as Box<dyn FnOnce() + Send>
            })
            .collect();

        let _results = execute_concurrent_raw(tasks, 2);
        // If counter leaks, subsequent tasks may fail to execute
        // Mainly verifies no deadlock occurs
    }

    #[test]
    fn test_return_guarantees_started_tasks_have_finished() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let finished = Arc::new(AtomicBool::new(false));
        let task_finished = Arc::clone(&finished);
        let tasks = vec![move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            task_finished.store(true, Ordering::Release);
        }];

        let results = execute_concurrent_raw(tasks, 1);

        assert_eq!(results.len(), 1);
        assert!(finished.load(Ordering::Acquire));
    }
}
