use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{GetLatestRepoError, Result};
use crate::models::ScanSource;
use anyhow::Context;

/// Synchronization configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    /// Whether to enable auto-sync (auto-scan new repos before fetching)
    #[serde(default = "default_auto_sync")]
    pub auto_sync: bool,
    /// Strict mode: true = scan when counts differ; false = only scan new paths
    #[serde(default = "default_strict_sync")]
    pub strict_sync: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            auto_sync: default_auto_sync(),
            strict_sync: default_strict_sync(),
        }
    }
}

fn default_auto_sync() -> bool {
    true
}

fn default_strict_sync() -> bool {
    false
}

/// App configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Scan source list
    pub scan_sources: Vec<ScanSource>,
    /// Default concurrency
    pub default_jobs: usize,
    /// Default timeout (seconds)
    pub default_timeout: u64,
    /// Default scan depth
    pub default_depth: usize,
    /// Ignore patterns
    pub ignore_patterns: Vec<String>,
    /// Sync configuration
    #[serde(default)]
    pub sync: SyncConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            scan_sources: Vec::new(),
            default_jobs: 5,
            default_timeout: 30,
            default_depth: 5,
            ignore_patterns: vec![
                ".git".to_string(),
                "node_modules".to_string(),
                "target".to_string(),
                "vendor".to_string(),
                ".idea".to_string(),
                ".vscode".to_string(),
            ],
            sync: SyncConfig::default(),
        }
    }
}

impl AppConfig {
    /// Get config directory
    pub fn config_dir() -> Result<PathBuf> {
        // 优先通过环境变量覆盖配置目录，便于测试隔离
        if let Ok(env_dir) = std::env::var("GETLATESTREPO_CONFIG_DIR") {
            let path = PathBuf::from(env_dir);
            if !path.exists() {
                std::fs::create_dir_all(&path)
                    .with_context(|| format!("无法创建配置目录: {}", path.display()))?;
            }
            return Ok(path);
        }
        let dir = dirs::config_dir()
            .context("无法获取配置目录")?
            .join("getlatestrepo");
        Ok(dir)
    }

    /// Get config file path
    pub fn config_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.toml"))
    }

    /// Ensure config directory exists
    fn ensure_config_dir() -> Result<()> {
        let dir = Self::config_dir()?;
        if !dir.exists() {
            fs::create_dir_all(&dir)
                .with_context(|| format!("无法创建配置目录: {}", dir.display()))?;
        }
        Ok(())
    }

    /// Load configuration
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("无法读取配置文件: {}", path.display()))?;

        let config: AppConfig = toml::from_str(&content)
            .with_context(|| format!("无法解析配置文件: {}", path.display()))?;

        Ok(config)
    }

    /// Save configuration
    ///
    /// Set config file permissions to 0600 (owner read/write only)
    pub fn save(&self) -> Result<()> {
        Self::ensure_config_dir()?;
        let path = Self::config_path()?;

        let content = toml::to_string_pretty(self).context("无法序列化配置")?;

        fs::write(&path, content)
            .with_context(|| format!("无法写入配置文件: {}", path.display()))?;

        // Set permissions to 0600 (owner read/write only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = fs::Permissions::from_mode(0o600);
            if let Err(e) = fs::set_permissions(&path, permissions) {
                eprintln!("警告：设置配置文件权限失败: {}", e);
            }
        }

        Ok(())
    }

    /// 校验并向内存配置添加扫描源，不立即写盘。
    ///
    /// 调用方必须先同步数据库，成功后再调用 [`Self::save`]，避免任一侧失败时
    /// 配置文件与数据库出现永久分歧。
    pub fn prepare_scan_source(&mut self, path: impl AsRef<Path>) -> Result<ScanSource> {
        let path = path.as_ref();
        let canonical = path
            .canonicalize()
            .with_context(|| format!("无法访问路径: {}", path.display()))?;

        let path_str = canonical.to_string_lossy().to_string();

        // Check if already exists
        if self.scan_sources.iter().any(|s| s.root_path == path_str) {
            return Err(GetLatestRepoError::DuplicatePath(path_str));
        }

        let source = ScanSource {
            root_path: path_str,
            max_depth: self.default_depth,
            ignore_patterns: self.ignore_patterns.clone(),
            ..Default::default()
        };

        self.scan_sources.push(source.clone());
        Ok(source)
    }

    /// 从内存配置中取出扫描源，不立即写盘，供跨配置与数据库的一致性操作使用。
    pub fn take_scan_source(&mut self, path_or_id: &str) -> Result<ScanSource> {
        let matched_index = if let Ok(id) = path_or_id.parse::<i64>() {
            self.scan_sources
                .iter()
                .position(|source| source.id == Some(id))
        } else {
            let path = Path::new(path_or_id);
            let matched_path = path
                .canonicalize()
                .map(|canonical| canonical.to_string_lossy().to_string())
                .unwrap_or_else(|_| path_or_id.to_string());
            self.scan_sources
                .iter()
                .position(|source| source.root_path == matched_path)
        };

        matched_index
            .map(|index| self.scan_sources.remove(index))
            .ok_or_else(|| GetLatestRepoError::SourceNotFound(path_or_id.to_string()))
    }

    /// 按界面显示的 1-based 编号从内存配置取出扫描源，但不立即写盘。
    pub fn take_scan_source_by_index(&mut self, index: usize) -> Result<ScanSource> {
        if index == 0 || index > self.scan_sources.len() {
            return Err(GetLatestRepoError::SourceNotFound(index.to_string()));
        }

        Ok(self.scan_sources.remove(index - 1))
    }

    /// 在内存中更新全局及现有扫描源的忽略规则，不立即写盘。
    pub fn apply_ignore_patterns(&mut self, patterns: Vec<String>) {
        self.ignore_patterns = patterns.clone();
        // 已有扫描源持有自己的 ignore_patterns。同步更新它们，保证
        // `config ignore` 对后续扫描立即生效，而不是只影响未来新增扫描源。
        for source in &mut self.scan_sources {
            source.ignore_patterns = patterns.clone();
        }
    }

    /// Check if configured
    pub fn is_initialized(&self) -> bool {
        !self.scan_sources.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_scan_source_by_index_only_mutates_memory() {
        let mut config = AppConfig {
            scan_sources: vec![
                ScanSource {
                    root_path: "/repos/first".to_string(),
                    ..ScanSource::default()
                },
                ScanSource {
                    root_path: "/repos/second".to_string(),
                    ..ScanSource::default()
                },
            ],
            ..AppConfig::default()
        };

        let removed = config.take_scan_source_by_index(1).unwrap();

        assert_eq!(removed.root_path, "/repos/first");
        assert_eq!(config.scan_sources.len(), 1);
        assert_eq!(config.scan_sources[0].root_path, "/repos/second");
    }

    #[test]
    fn prepare_scan_source_only_mutates_memory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let expected_path = temp_dir.path().canonicalize().unwrap();
        let mut config = AppConfig::default();

        let source = config.prepare_scan_source(temp_dir.path()).unwrap();

        assert_eq!(source.root_path, expected_path.to_string_lossy());
        assert_eq!(config.scan_sources.len(), 1);
        assert_eq!(config.scan_sources[0].root_path, source.root_path);
        assert_eq!(config.scan_sources[0].max_depth, source.max_depth);
    }
}
