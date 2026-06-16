//! Fetch command handling

use anyhow::Result;
use colored::Colorize;

use crate::commands::ensure_initialized;
use crate::fetcher::Fetcher;
use crate::git::ProxyConfig;
use crate::scanner::Scanner;

fn auto_sync_enabled(config: &crate::config::AppConfig) -> bool {
    config.sync.auto_sync
}

/// Execute fetch command
pub async fn execute(
    jobs: usize,
    timeout: u64,
    no_security_check: bool,
    auto_skip_high_risk: bool,
    proxy_config: Option<ProxyConfig>,
) -> Result<()> {
    let (config, db) = ensure_initialized()?;

    if auto_sync_enabled(&config) {
        let sync = crate::sync::RepoSync::new(config.sync.auto_sync);
        let sync_status = sync.ensure_synced(&config.scan_sources, &db, true).await?;
        if sync_status.needs_scan() {
            println!("{} {}", "ℹ".blue(), sync_status.description());
        }
    }

    let mut repos = db.list_repositories()?;

    if repos.is_empty() {
        let scanned = Scanner::scan_all(&config.scan_sources, &db, true, jobs).await?;
        if scanned.is_empty() {
            println!(
                "{} 数据库中没有仓库记录，请先运行：getlatestrepo scan",
                "!".yellow()
            );
            return Ok(());
        }
        repos = scanned;
    }

    println!(
        "{} 开始 fetch {} 个仓库（并发：{}，超时：{} 秒）...",
        "▶".cyan(),
        repos.len(),
        jobs,
        timeout
    );

    let mut fetcher = Fetcher::new(jobs, timeout)
        .with_security_scan(!no_security_check)
        .with_auto_skip_high_risk(auto_skip_high_risk);

    if let Some(ref proxy) = proxy_config {
        if proxy.enabled {
            fetcher = fetcher.with_proxy(proxy.clone());
            println!("{} 使用代理：{}", "ℹ".blue(), proxy.http_proxy);
        } else {
            println!(
                "{} 已配置代理但未启用：{}（使用 --proxy 启用）",
                "ℹ".blue(),
                proxy.http_proxy
            );
        }
    }

    let summary = fetcher.fetch_and_update(&repos, &db, true).await?;

    summary.print_summary();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_uses_configured_auto_sync_flag() {
        let mut config = crate::config::AppConfig::default();

        config.sync.auto_sync = false;
        assert!(!auto_sync_enabled(&config));

        config.sync.auto_sync = true;
        assert!(auto_sync_enabled(&config));
    }
}
