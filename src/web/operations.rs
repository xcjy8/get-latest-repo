use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use uuid::Uuid;

use super::dto::{
    FetchReadinessDto, OperationCountersDto, OperationDto, OperationKind, OperationState,
};
use super::{WebState, publish_repository_snapshot};
use crate::config::AppConfig;
use crate::db::{Database, OperationBatchRecord, OperationItemRecord};
use crate::fetcher::Fetcher;
use crate::models::{Freshness, Repository};
use crate::scanner::Scanner;
use crate::workflow::{BuiltInWorkflows, RepositoryExecutionIssue, WorkflowExecutor, WorkflowStep};

#[derive(Clone)]
pub struct OperationManager {
    inner: Arc<OperationManagerInner>,
}

struct OperationManagerInner {
    current: Mutex<Option<OperationDto>>,
    cancellation_requested: AtomicBool,
    persist: bool,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl OperationManager {
    pub fn new() -> Result<Self> {
        let database = Database::open()?;
        let current = database
            .latest_operation_batch()?
            .map(operation_from_record)
            .transpose()?;
        let manager = Self {
            inner: Arc::new(OperationManagerInner {
                current: Mutex::new(current),
                cancellation_requested: AtomicBool::new(false),
                persist: true,
                worker: Mutex::new(None),
            }),
        };
        if manager
            .current()
            .is_some_and(|operation| operation.state.is_active())
        {
            manager.update(|operation| {
                operation.state = OperationState::Interrupted;
                operation.message = "服务重启，上一操作已中断".to_string();
                operation.details =
                    vec!["未确认完成的仓库不会计为成功；请重新执行对应步骤".to_string()];
                operation.finished_at = Some(chrono::Local::now().to_rfc3339());
            })?;
        }
        Ok(manager)
    }

    #[cfg(test)]
    pub fn new_for_test() -> Self {
        Self {
            inner: Arc::new(OperationManagerInner {
                current: Mutex::new(None),
                cancellation_requested: AtomicBool::new(false),
                persist: false,
                worker: Mutex::new(None),
            }),
        }
    }

    pub fn current(&self) -> Option<OperationDto> {
        self.inner
            .current
            .lock()
            .ok()
            .and_then(|operation| operation.clone())
    }

    pub(super) fn existing_request(
        &self,
        request_id: &str,
        kind: OperationKind,
    ) -> Result<Option<OperationDto>> {
        if !self.inner.persist {
            return Ok(None);
        }
        let Some(record) = Database::open()?.operation_batch_by_request_id(request_id)? else {
            return Ok(None);
        };
        if record.kind != kind.as_str() {
            anyhow::bail!("同一请求 ID 不能用于不同操作");
        }
        Ok(Some(operation_from_record(record)?))
    }

    pub fn start(
        &self,
        kind: OperationKind,
        total: usize,
        request_id: String,
        source_batch_id: Option<String>,
    ) -> Result<(OperationDto, bool)> {
        if self.inner.persist
            && let Some(existing) = Database::open()?.operation_batch_by_request_id(&request_id)?
        {
            if existing.kind != kind.as_str() {
                anyhow::bail!("同一请求 ID 不能用于不同操作");
            }
            return Ok((operation_from_record(existing)?, false));
        }
        let mut current = self
            .inner
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("操作状态锁已损坏"))?;
        if current
            .as_ref()
            .is_some_and(|operation| operation.state.is_active())
        {
            anyhow::bail!("已有仓库操作正在运行");
        }
        self.inner
            .cancellation_requested
            .store(false, Ordering::Release);
        let operation = OperationDto {
            operation_id: Uuid::new_v4().to_string(),
            kind,
            state: OperationState::Queued,
            message: format!("{}已进入队列", kind.label()),
            details: Vec::new(),
            counters: OperationCountersDto::default(),
            completed: 0,
            total,
            request_id,
            source_batch_id,
            started_at: None,
            finished_at: None,
        };
        self.persist(&operation)?;
        *current = Some(operation.clone());
        Ok((operation, true))
    }

    pub fn request_cancel(&self, operation_id: &str) -> Result<OperationDto> {
        let mut current = self
            .inner
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("操作状态锁已损坏"))?;
        let operation = current
            .as_mut()
            .filter(|operation| operation.operation_id == operation_id)
            .ok_or_else(|| anyhow::anyhow!("未找到指定操作"))?;
        if !operation.state.is_active() {
            anyhow::bail!("操作已经结束，无法取消");
        }
        self.inner
            .cancellation_requested
            .store(true, Ordering::Release);
        operation.message = "已请求取消，正在等待当前安全步骤结束".to_string();
        let operation = operation.clone();
        self.persist(&operation)?;
        Ok(operation)
    }

    fn cancellation_requested(&self) -> bool {
        self.inner.cancellation_requested.load(Ordering::Acquire)
    }

    fn update(&self, update: impl FnOnce(&mut OperationDto)) -> Result<OperationDto> {
        let mut current = self
            .inner
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("操作状态锁已损坏"))?;
        let operation = current
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("操作状态不存在"))?;
        update(operation);
        let operation = operation.clone();
        self.persist(&operation)?;
        Ok(operation)
    }

    /// 顶层线程创建失败时立即收敛队列状态，避免控制台永久显示“运行中”。
    pub(super) fn mark_start_failed(&self, message: String) -> Result<OperationDto> {
        self.update(|operation| {
            operation.state = OperationState::Failed;
            operation.message = message;
            operation.finished_at = Some(chrono::Local::now().to_rfc3339());
        })
    }

    fn persist(&self, operation: &OperationDto) -> Result<()> {
        if self.inner.persist {
            persist_operation(operation)?;
        }
        Ok(())
    }

    pub(super) fn register_worker(&self, worker: std::thread::JoinHandle<()>) -> Result<()> {
        let mut slot = self
            .inner
            .worker
            .lock()
            .map_err(|_| anyhow::anyhow!("操作线程锁已损坏"))?;
        if let Some(previous) = slot.take() {
            if !previous.is_finished() {
                *slot = Some(previous);
                anyhow::bail!("上一操作线程尚未退出");
            }
            let _ = previous.join();
        }
        *slot = Some(worker);
        Ok(())
    }

    /// 服务退出前请求取消并等待顶层操作线程，确保没有后台任务继续修改仓库。
    pub(super) fn shutdown_and_join(&self) {
        self.inner
            .cancellation_requested
            .store(true, Ordering::Release);
        let worker = self
            .inner
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }
}

fn operation_from_record(record: OperationBatchRecord) -> Result<OperationDto> {
    let kind = OperationKind::from_str(&record.kind)
        .ok_or_else(|| anyhow::anyhow!("数据库包含未知操作类型：{}", record.kind))?;
    let state = OperationState::from_str(&record.state)
        .ok_or_else(|| anyhow::anyhow!("数据库包含未知操作状态：{}", record.state))?;
    Ok(OperationDto {
        operation_id: record.batch_id,
        kind,
        state,
        message: record.message,
        details: serde_json::from_str(&record.details_json)
            .map_err(|error| anyhow::anyhow!("操作明细损坏：{error}"))?,
        counters: OperationCountersDto {
            succeeded: record.succeeded,
            failed: record.failed,
            partial: record.partial,
            no_action: record.no_action,
            skipped: record.skipped,
        },
        completed: record.completed,
        total: record.total,
        request_id: record.request_id,
        source_batch_id: record.source_batch_id,
        started_at: record.started_at,
        finished_at: record.finished_at,
    })
}

fn operation_to_record(operation: &OperationDto) -> Result<OperationBatchRecord> {
    Ok(OperationBatchRecord {
        batch_id: operation.operation_id.clone(),
        request_id: operation.request_id.clone(),
        kind: operation.kind.as_str().to_string(),
        state: operation.state.as_str().to_string(),
        message: operation.message.clone(),
        details_json: serde_json::to_string(&operation.details)?,
        total: operation.total,
        completed: operation.completed,
        succeeded: operation.counters.succeeded,
        failed: operation.counters.failed,
        partial: operation.counters.partial,
        no_action: operation.counters.no_action,
        skipped: operation.counters.skipped,
        source_batch_id: operation.source_batch_id.clone(),
        started_at: operation.started_at.clone(),
        finished_at: operation.finished_at.clone(),
    })
}

fn persist_operation(operation: &OperationDto) -> Result<()> {
    Database::open()?.upsert_operation_batch(&operation_to_record(operation)?)
}

const FETCH_SCOPE_VALID_MINUTES: i64 = 30;

/// 返回可供第二步复用的最新 Fetch 批次及其精确成功仓库集合。
pub(super) fn reusable_fetch_batch(
    database: &Database,
) -> Result<Option<(OperationBatchRecord, Vec<Repository>)>> {
    let Some(batch) = database.latest_fetch_batch()? else {
        return Ok(None);
    };
    let Some(finished_at) = batch.finished_at.as_deref() else {
        return Ok(None);
    };
    let finished_at = chrono::DateTime::parse_from_rfc3339(finished_at)?;
    if chrono::Utc::now().signed_duration_since(finished_at.with_timezone(&chrono::Utc))
        > chrono::Duration::minutes(FETCH_SCOPE_VALID_MINUTES)
    {
        return Ok(None);
    }
    if database.fetch_batch_has_consumer(&batch.batch_id)? {
        return Ok(None);
    }
    let repositories = database.successful_fetch_repositories(&batch.batch_id)?;
    if repositories.is_empty() {
        return Ok(None);
    }
    Ok(Some((batch, repositories)))
}

pub(super) fn fetch_readiness(database: &Database) -> Result<FetchReadinessDto> {
    let Some((batch, repositories)) = reusable_fetch_batch(database)? else {
        return Ok(FetchReadinessDto::default());
    };
    let expires_at = batch.finished_at.as_deref().map(|finished_at| {
        chrono::DateTime::parse_from_rfc3339(finished_at)
            .map(|time| (time + chrono::Duration::minutes(FETCH_SCOPE_VALID_MINUTES)).to_rfc3339())
    });
    Ok(FetchReadinessDto {
        batch_id: Some(batch.batch_id),
        succeeded: repositories.len(),
        failed: batch.failed,
        ready: true,
        expires_at: expires_at.transpose()?,
    })
}

pub async fn execute_operation(state: WebState, queued: OperationDto) {
    let running = state.operations.update(|operation| {
        operation.state = OperationState::Running;
        operation.started_at = Some(chrono::Local::now().to_rfc3339());
        operation.message = format!("正在{}", operation.kind.label());
    });
    let Ok(running) = running else {
        return;
    };
    state.events.publish("operation.patch", &running);

    let result = run_operation(&state, &queued).await;
    let cancelled = state.operations.cancellation_requested();
    let final_operation = state.operations.update(|operation| {
        operation.finished_at = Some(chrono::Local::now().to_rfc3339());
        if cancelled {
            operation.state = OperationState::Cancelled;
            if let Ok(outcome) = &result {
                operation.completed = operation.total;
                operation.counters = outcome.counters.clone();
                operation.details = outcome.details.clone();
            } else {
                operation.counters.skipped = operation.total.saturating_sub(operation.completed);
            }
            operation.message = format!(
                "操作已取消：成功 {}，失败 {}，跳过 {}",
                operation.counters.succeeded, operation.counters.failed, operation.counters.skipped
            );
        } else {
            match &result {
                Ok(outcome) => {
                    operation.completed = operation.total;
                    operation.counters = outcome.counters.clone();
                    operation.message = outcome.message.clone();
                    operation.details = outcome.details.clone();
                    operation.state =
                        if outcome.counters.failed == operation.total && operation.total > 0 {
                            OperationState::Failed
                        } else if outcome.counters.failed > 0 || outcome.counters.partial > 0 {
                            OperationState::PartialFailed
                        } else {
                            OperationState::Succeeded
                        };
                }
                Err(error) => {
                    operation.state = OperationState::Failed;
                    operation.message = format!("{}失败", operation.kind.label());
                    operation.details = vec![error.to_string()];
                }
            }
        }
    });

    if let Ok(operation) = final_operation {
        state.events.publish("operation.patch", &operation);
    }
    if publish_repository_snapshot(&state).await.is_err() {
        state.events.publish(
            "resync.required",
            &serde_json::json!({ "reason": "snapshot_failed" }),
        );
    }
}

struct ExecutionOutcome {
    message: String,
    details: Vec<String>,
    counters: OperationCountersDto,
}

impl ExecutionOutcome {
    fn completed(kind: OperationKind) -> Self {
        Self {
            message: format!("{}完成", kind.label()),
            details: Vec::new(),
            counters: OperationCountersDto::default(),
        }
    }
}

/// 用当前 Fetch 批次成功路径限定第二步，防止失败仓库被旧时间戳误纳入。
#[cfg(test)]
fn repositories_in_fetch_scope(
    repositories: Vec<Repository>,
    successful_paths: &[String],
) -> Vec<Repository> {
    let successful_paths = successful_paths
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    repositories
        .into_iter()
        .filter(|repository| successful_paths.contains(repository.path.as_str()))
        .collect()
}

/// 将仓库级进度压缩到最高 20Hz；既保持实时反馈，也避免淹没 SSE 与 React。
fn repository_progress_observer(
    state: WebState,
    phase: &'static str,
) -> Arc<dyn Fn(usize, usize) + Send + Sync> {
    let last_emit = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(1)));
    Arc::new(move |completed: usize, total: usize| {
        let should_emit = last_emit.lock().is_ok_and(|mut last_emit| {
            if completed != total && last_emit.elapsed() < Duration::from_millis(50) {
                return false;
            }
            *last_emit = Instant::now();
            true
        });
        if should_emit
            && let Ok(operation) = state.operations.update(|operation| {
                operation.completed = completed;
                operation.total = total;
                operation.message = format!("{phase}：{completed}/{total}");
            })
        {
            state.events.publish("operation.patch", &operation);
        }
    })
}

/// 阶段切换时使用独立计数，让安全扫描等准备工作不再长期停留在 0。
fn phase_progress_observer(
    state: WebState,
) -> Arc<dyn Fn(&'static str, usize, usize) + Send + Sync> {
    let last_emit = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(1)));
    Arc::new(move |phase: &'static str, completed: usize, total: usize| {
        let should_emit = last_emit.lock().is_ok_and(|mut last_emit| {
            if completed != total && last_emit.elapsed() < Duration::from_millis(50) {
                return false;
            }
            *last_emit = Instant::now();
            true
        });
        if should_emit
            && let Ok(operation) = state.operations.update(|operation| {
                operation.completed = completed;
                operation.total = total;
                operation.message = format!("{phase}：{completed}/{total}");
            })
        {
            state.events.publish("operation.patch", &operation);
        }
    })
}

async fn run_operation(state: &WebState, queued: &OperationDto) -> Result<ExecutionOutcome> {
    let kind = queued.kind;
    if state.operations.cancellation_requested() {
        return Ok(ExecutionOutcome::completed(kind));
    }
    let config = AppConfig::load()?;
    if !config.is_initialized() {
        anyhow::bail!("尚未初始化扫描源");
    }
    let concurrency = crate::concurrent::AdaptiveConcurrency::detect(config.default_jobs);
    let db = Database::open()?;

    match kind {
        OperationKind::Scan => {
            let sources = config
                .scan_sources
                .iter()
                .filter(|source| source.enabled)
                .collect::<Vec<_>>();
            for (index, source) in sources.iter().enumerate() {
                if state.operations.cancellation_requested() {
                    break;
                }
                if let Ok(operation) = state.operations.update(|operation| {
                    operation.completed = index;
                    operation.total = sources.len();
                    operation.message = format!("正在扫描：{}", source.root_path);
                }) {
                    state.events.publish("operation.patch", &operation);
                }
                Scanner::scan_source(
                    source,
                    &db,
                    false,
                    concurrency.io_jobs,
                    Some(repository_progress_observer(state.clone(), "正在扫描仓库")),
                )
                .await?;
            }
        }
        OperationKind::Fetch => {
            let repositories = db.list_repositories()?;
            let cancellation_state = state.operations.clone();
            let summary = Fetcher::new(concurrency.fetch_jobs, config.default_timeout)
                .with_security_scan(state.security_check)
                .with_auto_allow_high_risk(true)
                .with_proxy(state.proxy.clone())
                .with_progress_observer(repository_progress_observer(
                    state.clone(),
                    "正在获取远程状态",
                ))
                .with_cancellation_checker(Arc::new(move || {
                    cancellation_state.cancellation_requested()
                }))
                .fetch_and_update(&repositories, &db, false)
                .await?;
            let fetched_paths = summary
                .results
                .iter()
                .filter(|result| result.success)
                .map(|result| result.repo_path.clone())
                .collect::<Vec<_>>();
            let reconcile_progress =
                repository_progress_observer(state.clone(), "正在复检获取结果");
            let (reconciled_paths, reconcile_errors) = tokio::task::spawn_blocking({
                let jobs = concurrency.io_jobs;
                move || reconcile_fetch_successes(fetched_paths, jobs, reconcile_progress)
            })
            .await??;
            for result in &summary.results {
                let reconciled = result.success && reconciled_paths.contains(&result.repo_path);
                let cancelled = result
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("取消"));
                let repository = db.get_repository(&result.repo_path)?;
                db.upsert_operation_item(&OperationItemRecord {
                    batch_id: queued.operation_id.clone(),
                    repo_id: repository.as_ref().and_then(|repository| repository.id),
                    repo_path: result.repo_path.clone(),
                    repo_name: repository
                        .as_ref()
                        .map(|repository| repository.name.clone())
                        .unwrap_or_else(|| result.repo_path.clone()),
                    stage: "fetch".to_string(),
                    outcome: if reconciled {
                        "succeeded".to_string()
                    } else if cancelled {
                        "skipped".to_string()
                    } else {
                        "failed".to_string()
                    },
                    error_code: (!reconciled).then(|| {
                        if cancelled {
                            "cancelled".to_string()
                        } else if result.success {
                            "reconcile_failed".to_string()
                        } else {
                            "fetch_failed".to_string()
                        }
                    }),
                    error_detail: result
                        .error
                        .clone()
                        .or_else(|| reconcile_errors.get(&result.repo_path).cloned()),
                    ..OperationItemRecord::default()
                })?;
            }
            let mut details = summary
                .results
                .iter()
                .filter(|result| {
                    !result.success
                        && !result
                            .error
                            .as_deref()
                            .is_some_and(|error| error.contains("取消"))
                })
                .map(|result| {
                    format!(
                        "{}：{}",
                        crate::utils::sanitize_path(&result.repo_path),
                        result.error.as_deref().unwrap_or("未知错误")
                    )
                })
                .collect::<Vec<_>>();
            details.extend(
                reconcile_errors
                    .iter()
                    .map(|(path, error)| format!("{}：{error}", crate::utils::sanitize_path(path))),
            );
            let succeeded = reconciled_paths.len();
            let missing = summary.total.saturating_sub(summary.results.len());
            let cancelled = summary
                .results
                .iter()
                .filter(|result| {
                    !result.success
                        && result
                            .error
                            .as_deref()
                            .is_some_and(|error| error.contains("取消"))
                })
                .count()
                + missing;
            let failed = summary.total.saturating_sub(succeeded + cancelled);
            if missing > 0 {
                details.push(if state.operations.cancellation_requested() {
                    format!("{missing} 个仓库因取消未开始")
                } else {
                    format!("{missing} 个仓库任务异常退出，未返回结果")
                });
            }
            return Ok(ExecutionOutcome {
                message: format!("获取完成：{} 个成功，{} 个失败", succeeded, failed),
                details,
                counters: OperationCountersDto {
                    succeeded,
                    failed,
                    skipped: cancelled,
                    ..OperationCountersDto::default()
                },
            });
        }
        OperationKind::Check
        | OperationKind::Daily
        | OperationKind::PullSafe
        | OperationKind::PullForce
        | OperationKind::PullBackup => {
            let workflow_name = match kind {
                OperationKind::Check => "check",
                OperationKind::Daily => "daily",
                OperationKind::PullSafe => "pull-safe",
                OperationKind::PullForce => "pull-force",
                OperationKind::PullBackup => "pull-backup",
                _ => unreachable!(),
            };
            let mut workflow = BuiltInWorkflows::get(workflow_name)
                .ok_or_else(|| anyhow::anyhow!("内置工作流不存在：{workflow_name}"))?;
            let target_repositories = if matches!(
                kind,
                OperationKind::PullSafe | OperationKind::PullForce | OperationKind::PullBackup
            ) {
                let source_batch_id = queued
                    .source_batch_id
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("操作缺少远程状态来源批次"))?;
                let repositories = db.successful_fetch_repositories(source_batch_id)?;
                if repositories.is_empty() {
                    anyhow::bail!("没有当前批次获取成功的仓库，请先执行第一步");
                }
                workflow
                    .steps
                    .retain(|step| !matches!(step, WorkflowStep::Fetch { .. }));
                if let Ok(operation) = state.operations.update(|operation| {
                    operation.completed = 0;
                    operation.total = repositories.len();
                    operation.message =
                        format!("正在复检 {} 个目标仓库与安全条件", repositories.len());
                }) {
                    state.events.publish("operation.patch", &operation);
                }
                Some(repositories)
            } else {
                None
            };
            for step in &mut workflow.steps {
                if let WorkflowStep::PullSafe { confirm, .. } = step {
                    *confirm = false;
                }
            }
            let cancellation_state = state.operations.clone();
            let mut executor = WorkflowExecutor::new(
                workflow,
                Some(concurrency.fetch_jobs),
                Some(config.default_timeout),
                false,
                true,
            )
            .with_security_check(state.security_check)
            .with_auto_allow_high_risk(true)
            .with_proxy(state.proxy.clone())
            .with_progress_observer(repository_progress_observer(state.clone(), "正在更新仓库"))
            .with_phase_progress_observer(phase_progress_observer(state.clone()))
            // Web 结束时会并发执行一次权威复检；跳过执行器内部串行重复扫描。
            .with_deferred_status_refresh(true)
            .with_cancellation_checker(Arc::new(move || {
                cancellation_state.cancellation_requested()
            }));
            if let Some(repositories) = target_repositories.clone() {
                executor = executor.with_target_repositories(repositories);
            }
            let result = executor.execute().await?;
            if matches!(
                kind,
                OperationKind::PullSafe | OperationKind::PullForce | OperationKind::PullBackup
            ) {
                let targets =
                    target_repositories.ok_or_else(|| anyhow::anyhow!("更新操作缺少目标仓库"))?;
                let operation_id = queued.operation_id.clone();
                let issue_messages = result
                    .repository_issues
                    .iter()
                    .map(RepositoryExecutionIssue::display_message)
                    .collect::<std::collections::HashSet<_>>();
                let batch_errors = result
                    .errors
                    .into_iter()
                    .filter(|error| !issue_messages.contains(error))
                    .collect();
                let repository_issues = result.repository_issues;
                let reconcile_progress =
                    repository_progress_observer(state.clone(), "正在核对最终状态");
                let (counters, details) = tokio::task::spawn_blocking(move || {
                    reconcile_pull_batch(
                        &operation_id,
                        targets,
                        repository_issues,
                        batch_errors,
                        concurrency.io_jobs,
                        reconcile_progress,
                    )
                })
                .await??;
                return Ok(ExecutionOutcome {
                    message: format!(
                        "更新完成：成功 {}，部分成功 {}，失败 {}，无需更新 {}",
                        counters.succeeded, counters.partial, counters.failed, counters.no_action
                    ),
                    details,
                    counters,
                });
            }
            if result.exit_code() != 0 {
                return Ok(ExecutionOutcome {
                    message: format!("{}未完整成功", kind.label()),
                    counters: OperationCountersDto {
                        failed: result.errors.len().max(1),
                        ..OperationCountersDto::default()
                    },
                    details: result.errors,
                });
            }
        }
    }
    Ok(ExecutionOutcome::completed(kind))
}

/// Fetch 成功后立刻重读 Git 状态，避免用旧 behind/dirty 数据驱动第二步。
fn reconcile_fetch_successes(
    paths: Vec<String>,
    jobs: usize,
    progress_observer: Arc<dyn Fn(usize, usize) + Send + Sync>,
) -> Result<(
    std::collections::HashSet<String>,
    std::collections::HashMap<String, String>,
)> {
    let total = paths.len();
    progress_observer(0, total);
    let database = Database::open()?;
    let mut candidates = Vec::new();
    let mut errors = std::collections::HashMap::new();
    for path in paths {
        match database.get_repository(&path)? {
            Some(repository) => candidates.push(repository),
            None => {
                errors.insert(path, "Fetch 后找不到数据库仓库记录".to_string());
            }
        }
    }
    let initial_completed = errors.len();
    progress_observer(initial_completed, total);
    let (task_repositories, results) = inspect_fetch_candidates(
        candidates,
        jobs,
        initial_completed,
        total,
        progress_observer,
    );
    let mut succeeded = std::collections::HashSet::new();
    for (repository, result) in task_repositories.into_iter().zip(results) {
        match result {
            Some((old, Ok(mut fresh))) => {
                fresh.id = old.id;
                fresh.last_fetch_at = old.last_fetch_at;
                fresh.last_pull_at = old.last_pull_at;
                database.upsert_repository(&mut fresh)?;
                succeeded.insert(old.path);
            }
            Some((old, Err(error))) => {
                errors.insert(old.path, format!("Fetch 后状态复检失败：{error}"));
            }
            None => {
                errors.insert(repository.path, "Fetch 后状态复检任务异常退出".to_string());
            }
        }
    }
    Ok((succeeded, errors))
}

type FetchInspectResult = Option<(Repository, crate::error::Result<Repository>)>;
type FetchInspectBatch = (Vec<Repository>, Vec<FetchInspectResult>);

/// 并发复检成功获取的仓库，并在每个实际完成点上报阶段进度。
fn inspect_fetch_candidates(
    candidates: Vec<Repository>,
    jobs: usize,
    initial_completed: usize,
    total: usize,
    progress_observer: Arc<dyn Fn(usize, usize) + Send + Sync>,
) -> FetchInspectBatch {
    let task_repositories = candidates.clone();
    let completed = Arc::new(AtomicUsize::new(initial_completed));
    let tasks = candidates
        .into_iter()
        .map(|repository| {
            let completed = Arc::clone(&completed);
            let progress_observer = Arc::clone(&progress_observer);
            move || {
                let result = crate::git::GitOps::refresh_remote_state_after_fetch(&repository);
                let current = completed.fetch_add(1, Ordering::AcqRel) + 1;
                progress_observer(current, total);
                (repository, result)
            }
        })
        .collect();
    let results = crate::concurrent::execute_concurrent_raw(tasks, jobs);
    (task_repositories, results)
}

/// 更新后逐仓库复检；最终 Git 状态是统计权威，执行过程错误只决定“部分成功/失败”。
fn reconcile_pull_batch(
    batch_id: &str,
    targets: Vec<Repository>,
    repository_issues: Vec<RepositoryExecutionIssue>,
    batch_errors: Vec<String>,
    jobs: usize,
    progress_observer: Arc<dyn Fn(usize, usize) + Send + Sync>,
) -> Result<(OperationCountersDto, Vec<String>)> {
    let database = Database::open()?;
    let mut counters = OperationCountersDto::default();
    let mut details = Vec::new();

    let total = targets.len();
    let (action_targets, no_action_targets): (Vec<_>, Vec<_>) =
        targets.into_iter().partition(repository_requires_action);
    counters.no_action = no_action_targets.len();
    for target in no_action_targets {
        database.upsert_operation_item(&OperationItemRecord {
            batch_id: batch_id.to_string(),
            repo_id: target.id,
            repo_path: target.path,
            repo_name: target.name,
            stage: "reconcile".to_string(),
            outcome: "no_action".to_string(),
            error_code: None,
            error_detail: None,
            parent_synced: true,
            final_dirty: false,
        })?;
    }

    progress_observer(counters.no_action, total);
    let task_targets = action_targets.clone();
    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(counters.no_action));
    let tasks = action_targets
        .into_iter()
        .map(|target| {
            let repository_errors = issue_messages_for_path(&repository_issues, &target.path);
            let completed = Arc::clone(&completed);
            let progress_observer = Arc::clone(&progress_observer);
            move || {
                let inspected = crate::git::GitOps::inspect(
                    std::path::Path::new(&target.path),
                    &target.root_path,
                );
                let current = completed.fetch_add(1, Ordering::AcqRel) + 1;
                progress_observer(current, total);
                (repository_errors, inspected)
            }
        })
        .collect();
    let results = crate::concurrent::execute_concurrent_raw(tasks, jobs);

    for (target, result) in task_targets.into_iter().zip(results) {
        let (repository_errors, inspected) = match result {
            Some(result) => result,
            None => {
                counters.failed += 1;
                let detail = format!("{}：更新后状态复检任务异常退出", target.name);
                details.push(detail.clone());
                database.upsert_operation_item(&OperationItemRecord {
                    batch_id: batch_id.to_string(),
                    repo_id: target.id,
                    repo_path: target.path,
                    repo_name: target.name,
                    stage: "reconcile".to_string(),
                    outcome: "failed".to_string(),
                    error_code: Some("reconcile_task_failed".to_string()),
                    error_detail: Some(detail),
                    parent_synced: false,
                    final_dirty: target.dirty,
                })?;
                continue;
            }
        };
        let (outcome, error_detail, parent_synced, final_dirty) = match inspected {
            Ok(mut fresh) => {
                fresh.id = target.id;
                fresh.last_fetch_at = target.last_fetch_at;
                let parent_synced = fresh.freshness != Freshness::HasUpdates;
                let required_action = repository_requires_action(&target);
                if parent_synced && target.freshness == Freshness::HasUpdates {
                    fresh.last_pull_at = Some(chrono::Local::now());
                } else {
                    fresh.last_pull_at = target.last_pull_at;
                }
                let final_dirty = fresh.dirty;
                database.upsert_repository(&mut fresh)?;

                if !required_action {
                    counters.no_action += 1;
                    ("no_action", None, parent_synced, final_dirty)
                } else if !repository_errors.is_empty() && parent_synced {
                    counters.partial += 1;
                    let detail = repository_errors.join("；");
                    details.push(detail.clone());
                    ("partial", Some(detail), true, final_dirty)
                } else if !repository_errors.is_empty() || !parent_synced {
                    counters.failed += 1;
                    let detail = if repository_errors.is_empty() {
                        format!("{}：更新后仍落后远程", target.name)
                    } else {
                        repository_errors.join("；")
                    };
                    details.push(detail.clone());
                    ("failed", Some(detail), parent_synced, final_dirty)
                } else {
                    counters.succeeded += 1;
                    ("succeeded", None, true, final_dirty)
                }
            }
            Err(error) => {
                counters.failed += 1;
                let detail = format!("{}：更新后状态复检失败：{error}", target.name);
                details.push(detail.clone());
                ("failed", Some(detail), false, target.dirty)
            }
        };
        database.upsert_operation_item(&OperationItemRecord {
            batch_id: batch_id.to_string(),
            repo_id: target.id,
            repo_path: target.path,
            repo_name: target.name,
            stage: "reconcile".to_string(),
            outcome: outcome.to_string(),
            error_code: error_detail.as_ref().map(|_| "update_issue".to_string()),
            error_detail,
            parent_synced,
            final_dirty,
        })?;
    }
    details.extend(batch_errors);
    Ok((counters, details))
}

fn repository_requires_action(repository: &Repository) -> bool {
    repository.freshness == Freshness::HasUpdates || repository.dirty
}

fn issue_messages_for_path(
    repository_issues: &[RepositoryExecutionIssue],
    path: &str,
) -> Vec<String> {
    repository_issues
        .iter()
        .filter(|issue| issue.path == path)
        .map(RepositoryExecutionIssue::display_message)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Freshness;

    fn repository(
        freshness: Freshness,
        last_fetch_at: Option<chrono::DateTime<chrono::Local>>,
    ) -> Repository {
        Repository {
            freshness,
            last_fetch_at,
            ..Repository::default()
        }
    }

    #[test]
    fn only_current_fetch_successes_enter_pull_scope() {
        let mut first = repository(Freshness::Synced, None);
        first.path = "/repos/first".to_string();
        let mut second = repository(Freshness::HasUpdates, None);
        second.path = "/repos/second".to_string();
        let mut failed = repository(Freshness::Unreachable, None);
        failed.path = "/repos/failed".to_string();

        let scoped = repositories_in_fetch_scope(
            vec![first, second, failed],
            &["/repos/first".to_string(), "/repos/second".to_string()],
        );

        assert_eq!(scoped.len(), 2);
        assert!(
            scoped
                .iter()
                .all(|repository| repository.path != "/repos/failed")
        );
    }

    #[test]
    fn empty_fetch_success_scope_selects_nothing() {
        assert!(
            repositories_in_fetch_scope(vec![repository(Freshness::Synced, None)], &[]).is_empty()
        );
    }

    #[test]
    fn shutdown_waits_for_registered_operation_worker() {
        let manager = OperationManager::new_for_test();
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            worker_finished.store(true, Ordering::Release);
        });
        manager.register_worker(worker).unwrap();

        manager.shutdown_and_join();

        assert!(finished.load(Ordering::Acquire));
    }

    #[test]
    fn duplicate_repository_names_are_attributed_by_exact_path() {
        let issues = vec![
            RepositoryExecutionIssue {
                name: "same-name".to_string(),
                path: "/repos/first".to_string(),
                message: "第一个失败".to_string(),
            },
            RepositoryExecutionIssue {
                name: "same-name".to_string(),
                path: "/repos/second".to_string(),
                message: "第二个失败".to_string(),
            },
        ];

        assert_eq!(
            issue_messages_for_path(&issues, "/repos/second"),
            vec!["same-name：第二个失败"]
        );
    }

    #[test]
    fn final_reconcile_skips_repositories_that_required_no_action() {
        assert!(!repository_requires_action(&repository(
            Freshness::Synced,
            None
        )));

        let mut dirty = repository(Freshness::Synced, None);
        dirty.dirty = true;
        assert!(repository_requires_action(&dirty));
        assert!(repository_requires_action(&repository(
            Freshness::HasUpdates,
            None
        )));
    }

    #[test]
    fn fetch_reconcile_reports_progress_for_each_completed_repository() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_by_callback = Arc::clone(&observed);
        let progress: Arc<dyn Fn(usize, usize) + Send + Sync> =
            Arc::new(move |completed, total| {
                observed_by_callback
                    .lock()
                    .unwrap()
                    .push((completed, total));
            });
        let candidate = Repository {
            path: "/definitely-missing/repository".to_string(),
            root_path: "/definitely-missing".to_string(),
            ..Repository::default()
        };

        let (_, results) = inspect_fetch_candidates(vec![candidate], 1, 0, 1, progress);

        assert_eq!(results.len(), 1);
        assert_eq!(observed.lock().unwrap().last(), Some(&(1, 1)));
    }
}
