//! Config command handling

use crate::cli::ConfigCommands;
use crate::config::AppConfig;
use crate::db::Database;
use anyhow::Result;
use colored::Colorize;

/// Execute config command
pub async fn execute(command: ConfigCommands) -> Result<()> {
    match command {
        ConfigCommands::Add { path } => {
            let mut config = AppConfig::load()?;
            config.add_scan_source(&path)?;

            // Sync to database
            let db = Database::open()?;
            if let Some(source) = config.scan_sources.last() {
                let mut source_clone = source.clone();
                db.upsert_scan_source(&mut source_clone)?;
            }

            println!("{} 已添加扫描源：{}", "✓".green(), path.display());
        }
        ConfigCommands::List => {
            let config = AppConfig::load()?;

            if config.scan_sources.is_empty() {
                println!("{} 暂未配置扫描源", "!".yellow());
            } else {
                println!("{} 已配置扫描源：\n", "ℹ".blue());
                for (idx, source) in config.scan_sources.iter().enumerate() {
                    println!("  {}. {}", idx + 1, source.root_path);
                    println!(
                        "     深度: {} | 忽略: {:?}",
                        source.max_depth, source.ignore_patterns
                    );
                }
            }
        }
        ConfigCommands::Remove { path_or_id } => {
            let mut config = AppConfig::load()?;
            let db = Database::open()?;

            // `config list` 展示的是 1-based 编号，不是 SQLite 内部 ID。
            // 先更新 TOML 配置；成功后再按 root_path 删除 DB 记录，避免半途失败造成不一致。
            let removed_path = if let Ok(index) = path_or_id.parse::<usize>() {
                config.remove_scan_source_by_index(index)?.root_path
            } else {
                let canonicalized =
                    if let Ok(path) = std::path::Path::new(&path_or_id).canonicalize() {
                        path.to_string_lossy().to_string()
                    } else {
                        path_or_id
                    };
                config.remove_scan_source(&canonicalized)?;
                canonicalized
            };

            db.delete_scan_source_by_path(&removed_path)?;
            println!("{} 已移除扫描源", "✓".green());
        }
        ConfigCommands::Ignore { patterns } => {
            let mut config = AppConfig::load()?;
            let pattern_list: Vec<String> =
                patterns.split(',').map(|s| s.trim().to_string()).collect();
            config.set_ignore_patterns(pattern_list.clone())?;
            let db = Database::open()?;
            for source in &config.scan_sources {
                let mut source_clone = source.clone();
                db.upsert_scan_source(&mut source_clone)?;
            }
            println!("{} 已设置忽略规则：{:?}", "✓".green(), pattern_list);
        }
        ConfigCommands::Path => {
            println!("配置文件：{}", AppConfig::config_path()?.display());
            println!("数据库：{}", Database::db_path()?.display());
        }
    }

    Ok(())
}
