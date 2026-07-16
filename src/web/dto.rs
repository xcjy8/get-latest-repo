use serde::{Deserialize, Serialize};

use crate::models::{Freshness, RepoSummary, Repository};

/// 前端仓库列表只携带首屏渲染所需字段，避免传输文件变更等大对象。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositorySummaryDto {
    pub repo_id: String,
    pub entity_version: u64,
    pub name: String,
    pub path: String,
    pub branch: Option<String>,
    pub dirty: bool,
    pub ahead_count: i32,
    pub behind_count: i32,
    pub freshness: &'static str,
    pub last_commit_at: Option<String>,
    pub last_commit_message: Option<String>,
    pub last_fetch_at: Option<String>,
    pub last_pull_at: Option<String>,
}

impl RepositorySummaryDto {
    pub fn from_repository(repository: Repository, entity_version: u64) -> Self {
        let freshness = match repository.freshness {
            Freshness::HasUpdates => "has_updates",
            Freshness::Synced => "synced",
            Freshness::Unreachable => "unreachable",
            Freshness::NoRemote => "no_remote",
        };
        Self {
            repo_id: repository
                .id
                .map(|id| id.to_string())
                .unwrap_or_else(|| repository.path.clone()),
            entity_version,
            name: repository.name,
            path: repository.path,
            branch: repository.branch,
            dirty: repository.dirty,
            ahead_count: repository.ahead_count,
            behind_count: repository.behind_count,
            freshness,
            last_commit_at: repository.last_commit_at.map(|value| value.to_rfc3339()),
            last_commit_message: repository.last_commit_message,
            last_fetch_at: repository.last_fetch_at.map(|value| value.to_rfc3339()),
            last_pull_at: repository.last_pull_at.map(|value| value.to_rfc3339()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatisticsDto {
    pub total: usize,
    pub has_updates: usize,
    pub synced: usize,
    pub unreachable: usize,
    pub no_remote: usize,
    pub dirty: usize,
}

impl From<&[Repository]> for StatisticsDto {
    fn from(repositories: &[Repository]) -> Self {
        let mut summary = RepoSummary::new();
        for repository in repositories {
            summary.add(repository);
        }
        Self {
            total: summary.total,
            has_updates: summary.has_updates,
            synced: summary.synced,
            unreachable: summary.unreachable,
            no_remote: summary.no_remote,
            dirty: summary.dirty,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OperationKind {
    Scan,
    Fetch,
    Check,
    Daily,
    PullSafe,
    PullForce,
    PullBackup,
}

impl OperationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Fetch => "fetch",
            Self::Check => "check",
            Self::Daily => "daily",
            Self::PullSafe => "pull-safe",
            Self::PullForce => "pull-force",
            Self::PullBackup => "pull-backup",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "scan" => Some(Self::Scan),
            "fetch" => Some(Self::Fetch),
            "check" => Some(Self::Check),
            "daily" => Some(Self::Daily),
            "pull-safe" => Some(Self::PullSafe),
            "pull-force" => Some(Self::PullForce),
            "pull-backup" => Some(Self::PullBackup),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Scan => "扫描仓库",
            Self::Fetch => "获取远程状态",
            Self::Check => "状态检查",
            Self::Daily => "日常工作流",
            Self::PullSafe => "安全更新",
            Self::PullForce => "暂存后强制更新",
            Self::PullBackup => "备份后更新",
        }
    }

    pub fn requires_confirmation(self) -> bool {
        matches!(self, Self::PullSafe | Self::PullForce | Self::PullBackup)
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Queued,
    Running,
    Succeeded,
    PartialFailed,
    Failed,
    Cancelled,
    Interrupted,
}

impl OperationState {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

impl OperationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::PartialFailed => "partial_failed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "partial_failed" => Some(Self::PartialFailed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "interrupted" => Some(Self::Interrupted),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationCountersDto {
    pub succeeded: usize,
    pub failed: usize,
    pub partial: usize,
    pub no_action: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationDto {
    pub operation_id: String,
    pub kind: OperationKind,
    pub state: OperationState,
    pub message: String,
    pub details: Vec<String>,
    pub counters: OperationCountersDto,
    pub completed: usize,
    pub total: usize,
    pub request_id: String,
    pub source_batch_id: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartOperationRequest {
    pub kind: OperationKind,
    pub request_id: String,
    #[serde(default)]
    pub confirmed: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapDto {
    pub version: &'static str,
    pub revision: String,
    pub event_sequence: String,
    pub csrf_token: String,
    pub repositories: Vec<RepositorySummaryDto>,
    pub statistics: StatisticsDto,
    pub active_operation: Option<OperationDto>,
    pub fetch_readiness: FetchReadinessDto,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchReadinessDto {
    pub batch_id: Option<String>,
    pub succeeded: usize,
    pub failed: usize,
    pub ready: bool,
    pub expires_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ErrorDto {
    pub message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryPatchDto {
    pub upserts: Vec<RepositorySummaryDto>,
    pub removes: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanSourceDto {
    pub index: usize,
    pub root_path: String,
    pub max_depth: usize,
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigDto {
    pub default_jobs: usize,
    pub effective_fetch_jobs: usize,
    pub effective_io_jobs: usize,
    pub logical_cpus: usize,
    pub memory_mib: Option<u64>,
    pub default_timeout: u64,
    pub default_depth: usize,
    pub ignore_patterns: Vec<String>,
    pub scan_sources: Vec<ScanSourceDto>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderSelectionDto {
    pub path: String,
}

impl From<crate::config::AppConfig> for ConfigDto {
    fn from(config: crate::config::AppConfig) -> Self {
        let concurrency = crate::concurrent::AdaptiveConcurrency::detect(config.default_jobs);
        Self {
            default_jobs: config.default_jobs,
            effective_fetch_jobs: concurrency.fetch_jobs,
            effective_io_jobs: concurrency.io_jobs,
            logical_cpus: concurrency.logical_cpus,
            memory_mib: concurrency.memory_mib,
            default_timeout: config.default_timeout,
            default_depth: config.default_depth,
            ignore_patterns: config.ignore_patterns,
            scan_sources: config
                .scan_sources
                .into_iter()
                .enumerate()
                .map(|(index, source)| ScanSourceDto {
                    index: index + 1,
                    root_path: source.root_path,
                    max_depth: source.max_depth,
                    enabled: source.enabled,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AddScanSourceRequest {
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct DiscardRequest {
    pub confirmed: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscardResultDto {
    pub discarded_files: Vec<String>,
}
