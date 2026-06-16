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

    /// Add scan source
    pub fn add_scan_source(&mut self, path: impl AsRef<Path>) -> Result<()> {
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

        self.scan_sources.push(source);
        self.save()?;

        Ok(())
    }

    /// Remove scan source
    pub fn remove_scan_source(&mut self, path_or_id: &str) -> Result<()> {
        let before = self.scan_sources.len();

        // Try parsing as ID
        if let Ok(id) = path_or_id.parse::<i64>() {
            self.scan_sources.retain(|s| s.id != Some(id));
        } else {
            // Handle as path
            let path = Path::new(path_or_id);
            if let Ok(canonical) = path.canonicalize() {
                let path_str = canonical.to_string_lossy().to_string();
                self.scan_sources.retain(|s| s.root_path != path_str);
            } else {
                // Try raw string matching
                self.scan_sources.retain(|s| s.root_path != path_or_id);
            }
        }

        if self.scan_sources.len() == before {
            return Err(GetLatestRepoError::SourceNotFound(path_or_id.to_string()));
        }

        self.save()?;
        Ok(())
    }

    /// Remove scan source by the 1-based index shown by `config list`.
    ///
    /// The TOML config is the durable user-facing source of scan roots, while SQLite
    /// stores its own internal row IDs. Keeping this index-based removal here prevents
    /// command code from guessing whether a numeric argument means a DB row ID or the
    /// displayed list number.
    pub fn remove_scan_source_by_index(&mut self, index: usize) -> Result<ScanSource> {
        if index == 0 || index > self.scan_sources.len() {
            return Err(GetLatestRepoError::SourceNotFound(index.to_string()));
        }

        let removed = self.scan_sources.remove(index - 1);
        self.save()?;
        Ok(removed)
    }

    /// Set ignore rules
    pub fn set_ignore_patterns(&mut self, patterns: Vec<String>) -> Result<()> {
        self.ignore_patterns = patterns.clone();
        // 已有扫描源持有自己的 ignore_patterns。同步更新它们，保证
        // `config ignore` 对后续扫描立即生效，而不是只影响未来新增扫描源。
        for source in &mut self.scan_sources {
            source.ignore_patterns = patterns.clone();
        }
        self.save()?;
        Ok(())
    }

    /// Check if configured
    pub fn is_initialized(&self) -> bool {
        !self.scan_sources.is_empty()
    }
}
