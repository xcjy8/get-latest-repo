//! Init command handling

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::PathBuf;

use crate::commands::{print_info, print_success};
use crate::config::AppConfig;
use crate::db::Database;

/// Execute init command
pub async fn execute(path: PathBuf) -> Result<()> {
    println!("{} 正在初始化 GetLatestRepo...", "▶".cyan());

    // 已存在但损坏的配置必须显式报错，禁止用默认值覆盖用户数据。
    let mut config = AppConfig::load().context("无法加载现有配置，初始化已安全中止")?;

    // Validate path
    let canonical = path
        .canonicalize()
        .with_context(|| format!("无法访问路径：{}", path.display()))?;

    let original_config = config.clone();
    let new_source = config.prepare_scan_source(&canonical)?;

    // 初始化数据库，并同步全部扫描源。
    let db = Database::open()?;
    let previous_database_source = db
        .list_scan_sources()?
        .into_iter()
        .find(|source| source.root_path == new_source.root_path);

    // 新配置最后写盘；任一同步失败时，磁盘配置仍保持原状。
    for source in &config.scan_sources {
        let mut source_clone = source.clone();
        db.upsert_scan_source(&mut source_clone)?;
    }
    if let Err(save_error) = config.save() {
        let config_rollback = original_config.save();
        let database_rollback = if let Some(mut previous) = previous_database_source {
            db.upsert_scan_source(&mut previous)
        } else {
            db.delete_scan_source_by_path(&new_source.root_path)
        };
        match (config_rollback, database_rollback) {
            (Ok(()), Ok(())) => return Err(save_error.into()),
            (config_result, database_result) => {
                anyhow::bail!(
                    "初始化失败且回滚不完整：保存错误={save_error}；配置回滚={:?}；数据库回滚={:?}",
                    config_result.err(),
                    database_result.err()
                );
            }
        }
    }

    print_success(&format!("已添加扫描源：{}", canonical.display()));
    print_info(&format!(
        "配置文件：{}",
        AppConfig::config_path()?.display()
    ));
    print_info(&format!("数据库：{}", Database::db_path()?.display()));
    println!();
    println!("{} 下一步:", "▶".cyan());
    println!("   1. 运行 `getlatestrepo scan` 扫描仓库");
    println!("   2. 运行 `getlatestrepo fetch` 检查远程更新");
    println!("   3. 运行 `getlatestrepo workflow daily` 执行自动化日常检查");

    Ok(())
}
