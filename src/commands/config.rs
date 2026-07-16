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
            let original_config = config.clone();
            let mut source = config.prepare_scan_source(&path)?;
            let db = Database::open()?;
            let previous_database_source = db
                .list_scan_sources()?
                .into_iter()
                .find(|existing| existing.root_path == source.root_path);
            db.upsert_scan_source(&mut source)?;

            if let Err(save_error) = config.save() {
                let config_rollback = original_config.save();
                let database_rollback = if let Some(mut previous) = previous_database_source {
                    db.upsert_scan_source(&mut previous)
                } else {
                    db.delete_scan_source_by_path(&source.root_path)
                };
                match (config_rollback, database_rollback) {
                    (Ok(()), Ok(())) => return Err(save_error.into()),
                    (config_result, database_result) => {
                        anyhow::bail!(
                            "添加扫描源失败且回滚不完整：保存错误={save_error}；配置回滚={:?}；数据库回滚={:?}",
                            config_result.err(),
                            database_result.err()
                        );
                    }
                }
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
            let original_config = config.clone();
            let db = Database::open()?;

            // `config list` 展示的是 1-based 编号，不是 SQLite 内部 ID。
            // 先只修改内存，再依次提交数据库与配置；第二步失败时补偿恢复两侧。
            let removed = if let Ok(index) = path_or_id.parse::<usize>() {
                config.take_scan_source_by_index(index)?
            } else {
                let canonicalized =
                    if let Ok(path) = std::path::Path::new(&path_or_id).canonicalize() {
                        path.to_string_lossy().to_string()
                    } else {
                        path_or_id
                    };
                config.take_scan_source(&canonicalized)?
            };

            db.delete_scan_source_by_path(&removed.root_path)?;
            if let Err(save_error) = config.save() {
                let config_rollback = original_config.save();
                let mut restored = removed.clone();
                restored.id = None;
                let database_rollback = db.upsert_scan_source(&mut restored);
                match (config_rollback, database_rollback) {
                    (Ok(()), Ok(())) => {
                        return Err(save_error.into());
                    }
                    (config_result, database_result) => {
                        anyhow::bail!(
                            "移除扫描源失败且回滚不完整：保存错误={save_error}；配置回滚={:?}；数据库回滚={:?}",
                            config_result.err(),
                            database_result.err()
                        );
                    }
                }
            }
            println!("{} 已移除扫描源", "✓".green());
        }
        ConfigCommands::Ignore { patterns } => {
            let mut config = AppConfig::load()?;
            let original_config = config.clone();
            let pattern_list: Vec<String> =
                patterns.split(',').map(|s| s.trim().to_string()).collect();
            config.apply_ignore_patterns(pattern_list.clone());
            let mut database = Database::open()?;
            let mut updated_sources = config.scan_sources.clone();
            if let Err(update_error) = database
                .commit_scan_sources_with(&mut updated_sources, || {
                    config.save().map_err(Into::into)
                })
            {
                if let Err(rollback_error) = original_config.save() {
                    anyhow::bail!(
                        "设置忽略规则失败且配置回滚失败：更新错误={update_error}；回滚错误={rollback_error}"
                    );
                }
                return Err(update_error);
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
