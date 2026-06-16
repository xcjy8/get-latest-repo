use thiserror::Error;

/// The unified entry point for all GetLatestRepo errors.
///
/// Each module returns `Result<T, GetLatestRepoError>` instead of `anyhow::Error`,
/// so callers can match error types precisely for differentiated handling.
#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum GetLatestRepoError {
    // ── I/O / Paths ───────────────────────────────────────────────
    #[error("路径不存在: {0}")]
    PathNotFound(String),

    #[error("路径无效: {0}")]
    InvalidPath(String),

    #[error("仓库路径不存在: {0}")]
    RepoPathMissing(String),

    // ── Git Operations ────────────────────────────────────────────
    #[error("不是有效的 Git 仓库: {0}")]
    NotGitRepo(String),

    #[error("无法打开仓库 {path}: {source}")]
    OpenRepo { path: String, source: git2::Error },

    #[error("需要认证 (401/403): {0}")]
    AuthRequired(String),

    #[error("仓库不存在或已转为私有 (404): {0}")]
    RepoNotFound(String),

    #[error("网络错误: {0}")]
    Network(String),

    #[error("当前不在任何分支上")]
    DetachedHead,

    #[error("远程分支不存在，请先运行 fetch")]
    RemoteBranchMissing,

    #[error("远程分支没有目标提交")]
    RemoteBranchNoTarget,

    #[error("Git 操作失败: {0}")]
    GitOperation(#[from] git2::Error),

    // ── Pull safety ───────────────────────────────────────────────
    #[error("检测到潜在仓库删除风险: {detail}")]
    RepoDeletionRisk { detail: String },

    #[error("安全检查失败: {source}")]
    SecurityCheckFailed { source: anyhow::Error },

    #[error("安全扫描失败，已跳过")]
    SecurityScanFailed,

    #[error("用户已取消")]
    UserCancelled,

    // ── Database ──────────────────────────────────────────────────
    #[error("数据库操作失败: {0}")]
    Database(#[from] rusqlite::Error),

    // ── Scan ──────────────────────────────────────────────────────
    #[error("扫描路径不存在: {0}")]
    ScanPathMissing(String),

    #[error("未找到仓库")]
    NoRepos,

    #[error("没有启用的扫描源")]
    NoSources,

    // ── Config ────────────────────────────────────────────────────
    #[error("尚未初始化，请先运行: getlatestrepo init <path>")]
    NotInitialized,

    #[error("路径已存在: {0}")]
    DuplicatePath(String),

    #[error("未找到匹配的扫描源: {0}")]
    SourceNotFound(String),

    // ── General IO ────────────────────────────────────────────────
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("WalkDir 错误: {0}")]
    WalkDir(#[from] walkdir::Error),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

/// Convenience type alias
pub type Result<T> = std::result::Result<T, GetLatestRepoError>;

/// Convert from FetchStatus to GetLatestRepoError
///
/// Only error statuses should be converted; Success should not be converted.
/// The caller should check for Success before converting.
impl TryFrom<crate::git::FetchStatus> for GetLatestRepoError {
    type Error = anyhow::Error;

    fn try_from(status: crate::git::FetchStatus) -> std::result::Result<Self, Self::Error> {
        use crate::git::FetchStatus;
        match status {
            FetchStatus::AuthenticationRequired { message } => {
                Ok(GetLatestRepoError::AuthRequired(message))
            }
            FetchStatus::RepositoryNotFound { message } => {
                Ok(GetLatestRepoError::RepoNotFound(message))
            }
            FetchStatus::NetworkError { message } => Ok(GetLatestRepoError::Network(message)),
            FetchStatus::OtherError { message } => {
                Ok(GetLatestRepoError::Other(anyhow::anyhow!(message)))
            }
            FetchStatus::Success => Err(anyhow::anyhow!(
                "不能将 FetchStatus::Success 转换为 GetLatestRepoError，请在转换前先检查状态"
            )),
        }
    }
}
