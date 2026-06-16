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

    let mut config = AppConfig::load().unwrap_or_default();

    // Validate path
    let canonical = path
        .canonicalize()
        .with_context(|| format!("无法访问路径：{}", path.display()))?;

    config.add_scan_source(&canonical)?;

    // Initialize the database and sync the scan source
    let db = Database::open()?;

    // Sync config scan sources to the database
    for source in &config.scan_sources {
        let mut source_clone = source.clone();
        db.upsert_scan_source(&mut source_clone)?;
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
