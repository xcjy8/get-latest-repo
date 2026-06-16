use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

/// Repository freshness status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Freshness {
    /// Has remote updates
    HasUpdates,
    /// Synced
    Synced,
    /// Remote unreachable
    Unreachable,
    /// No upstream branch
    NoRemote,
}

impl Freshness {
    pub fn as_str(&self) -> &'static str {
        match self {
            Freshness::HasUpdates => "has_updates",
            Freshness::Synced => "synced",
            Freshness::Unreachable => "unreachable",
            Freshness::NoRemote => "no_remote",
        }
    }

    pub fn emoji(&self) -> &'static str {
        match self {
            Freshness::HasUpdates => "🔴",
            Freshness::Synced => "🟢",
            Freshness::Unreachable => "⚫",
            Freshness::NoRemote => "⚪",
        }
    }
}

impl From<&str> for Freshness {
    fn from(s: &str) -> Self {
        match s {
            "has_updates" => Freshness::HasUpdates,
            "synced" => Freshness::Synced,
            "unreachable" => Freshness::Unreachable,
            "no_remote" => Freshness::NoRemote,
            _ => Freshness::NoRemote,
        }
    }
}

/// Scan source configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanSource {
    pub id: Option<i64>,
    pub root_path: String,
    pub max_depth: usize,
    pub ignore_patterns: Vec<String>,
    pub follow_symlinks: bool,
    pub enabled: bool,
    pub last_scan_at: Option<DateTime<Local>>,
}

impl Default for ScanSource {
    fn default() -> Self {
        Self {
            id: None,
            root_path: String::new(),
            max_depth: 5,
            ignore_patterns: vec![
                ".git".to_string(),
                "node_modules".to_string(),
                "target".to_string(),
            ],
            follow_symlinks: false,
            enabled: true,
            last_scan_at: None,
        }
    }
}

/// Full repository info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    pub id: Option<i64>,
    pub path: String,
    pub root_path: String,
    pub name: String,
    pub depth: u32,

    // Git Status
    pub branch: Option<String>,
    pub dirty: bool,
    /// Changed file list (detailed metadata)
    #[serde(skip)] // Not serialized to database, regenerated during scan
    pub file_changes: Vec<FileChange>,
    /// Changed file path list (database compatibility, deprecated)
    #[serde(rename = "dirty_files")]
    pub dirty_files: Vec<String>,
    pub upstream_ref: Option<String>,
    pub upstream_url: Option<String>,

    // Sync status
    pub ahead_count: i32,
    pub behind_count: i32,
    pub freshness: Freshness,

    // Timestamps
    pub last_commit_at: Option<DateTime<Local>>,
    pub last_commit_message: Option<String>,
    pub last_commit_author: Option<String>,
    pub last_scanned_at: Option<DateTime<Local>>,
    pub last_fetch_at: Option<DateTime<Local>>,
    pub last_pull_at: Option<DateTime<Local>>,
}

impl Repository {
    /// Create repository instance with new path (for needauth move scenario)
    ///
    /// Reuse all other fields, only update path-related info.
    /// Depth is recalculated based on new_path relative to new_root_path.
    pub fn with_new_path(self, new_path: String, new_root_path: String) -> Self {
        let depth = std::path::Path::new(&new_path)
            .strip_prefix(&new_root_path)
            .map(|p| p.components().count() as u32)
            .unwrap_or(0);
        Self {
            path: new_path,
            root_path: new_root_path,
            depth,
            ..self
        }
    }

    /// Get change statistics summary
    pub fn change_summary(&self) -> String {
        if self.file_changes.is_empty() {
            return format!("{} 个文件有变更", self.dirty_files.len());
        }

        let staged = self.file_changes.iter().filter(|fc| fc.staged).count();
        let unstaged = self.file_changes.len() - staged;

        if staged > 0 && unstaged > 0 {
            format!("{} 个已暂存，{} 个未暂存", staged, unstaged)
        } else if staged > 0 {
            format!("{} 个已暂存", staged)
        } else {
            format!("{} 个未暂存", unstaged)
        }
    }
}

impl Default for Repository {
    fn default() -> Self {
        Self {
            id: None,
            path: String::new(),
            root_path: String::new(),
            name: String::new(),
            depth: 0,
            branch: None,
            dirty: false,
            file_changes: Vec::new(),
            dirty_files: Vec::new(),
            upstream_ref: None,
            upstream_url: None,
            ahead_count: 0,
            behind_count: 0,
            freshness: Freshness::Synced,
            last_commit_at: None,
            last_commit_message: None,
            last_commit_author: None,
            last_scanned_at: None,
            last_fetch_at: None,
            last_pull_at: None,
        }
    }
}

/// Repository status summary (for quick display)
#[derive(Debug, Clone, Default)]
pub struct RepoSummary {
    pub total: usize,
    pub has_updates: usize,
    pub synced: usize,
    pub unreachable: usize,
    pub no_remote: usize,
    pub dirty: usize,
}

impl RepoSummary {
    pub fn new() -> Self {
        Self {
            total: 0,
            has_updates: 0,
            synced: 0,
            unreachable: 0,
            no_remote: 0,
            dirty: 0,
        }
    }

    pub fn add(&mut self, repo: &Repository) {
        self.total += 1;
        match repo.freshness {
            Freshness::HasUpdates => self.has_updates += 1,
            Freshness::Synced => self.synced += 1,
            Freshness::Unreachable => self.unreachable += 1,
            Freshness::NoRemote => self.no_remote += 1,
        }
        if repo.dirty {
            self.dirty += 1;
        }
    }
}

/// Fetch task results
#[derive(Debug, Clone)]
pub struct FetchResult {
    pub repo_path: String,
    pub success: bool,
    pub error: Option<String>,
    pub duration_ms: u64,
    /// Number of retries performed for network errors
    pub retry_count: u32,
}

/// File change info
#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    /// File path
    pub path: String,
    /// Change status: modified, added, deleted, renamed, typechange
    pub status: String,
    /// Whether in staging area
    pub staged: bool,
    /// Change impact description (for display)
    pub impact: String,
    /// Predicted result after executing stash
    pub stash_effect: String,
}

impl FileChange {
    /// Create file change info
    pub fn new(path: impl Into<String>, status: impl Into<String>, staged: bool) -> Self {
        let path = path.into();
        let status = status.into();
        let (impact, stash_effect) = Self::describe_change(&status, staged);

        Self {
            path,
            status,
            staged,
            impact,
            stash_effect,
        }
    }

    /// Describe change impact and stash effect
    fn describe_change(status: &str, staged: bool) -> (String, String) {
        let stage_info = if staged { "（已暂存）" } else { "" };

        match status {
            "added" => {
                let impact = format!("{}新增文件，将进入提交", stage_info);
                let stash = "stash 后：文件会暂时消失，pop 后恢复（新增文件可能需要重新 add）";
                (impact, stash.to_string())
            }
            "modified" => {
                let impact = format!("{}内容已修改", stage_info);
                let stash = "stash 后：变更会暂时消失，pop 后恢复";
                (impact, stash.to_string())
            }
            "deleted" => {
                let impact = format!("{}文件已删除", stage_info);
                let stash = "stash 后：文件会暂时恢复，pop 后再次删除";
                (impact, stash.to_string())
            }
            "renamed" => {
                let impact = format!("{}文件已重命名", stage_info);
                let stash = "stash 后：暂时恢复原文件名，pop 后恢复重命名";
                (impact, stash.to_string())
            }
            "untracked" => {
                let impact = "未跟踪的新文件（不会进入提交）".to_string();
                let stash = "stash -u 后：文件会暂时消失，pop 后恢复";
                (impact, stash.to_string())
            }
            "ignored" => {
                let impact = "文件已被 .gitignore 忽略".to_string();
                let stash = "stash 不会影响此文件".to_string();
                (impact, stash.to_string())
            }
            _ => {
                let impact = format!("{}未知变更", stage_info);
                let stash = "stash 影响未知，建议手动检查";
                (impact, stash.to_string())
            }
        }
    }
}
