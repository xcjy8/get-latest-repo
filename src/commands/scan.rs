//! Scan command handling

use anyhow::Result;
use colored::Colorize;
use std::path::PathBuf;
use std::time::Instant;

use crate::cli::OutputFormat;
use crate::commands::ensure_initialized;
// Database returned by ensure_initialized
use crate::fetcher::Fetcher;
use crate::git::ProxyConfig;
use crate::models::RepoSummary;
use crate::reporter::{
    Reporter, html::HtmlReporter, markdown::MarkdownReporter, save_report_async,
    terminal::TerminalReporter, terminal::print_scan_summary,
};
use crate::scanner::Scanner;

/// 扫描命令的完整运行参数，集中传递可避免新增网络能力时遗漏配置。
pub struct ScanOptions {
    pub should_fetch: bool,
    pub format: OutputFormat,
    pub out_path: Option<PathBuf>,
    pub depth: Option<usize>,
    pub jobs: usize,
    pub no_security_check: bool,
    pub auto_skip_high_risk: bool,
    pub proxy_config: Option<ProxyConfig>,
}

/// Execute scan command
pub async fn execute(options: ScanOptions) -> Result<()> {
    let ScanOptions {
        should_fetch,
        format,
        out_path,
        depth,
        jobs,
        no_security_check,
        auto_skip_high_risk,
        proxy_config,
    } = options;
    let start = Instant::now();

    let (config, db) = ensure_initialized()?;

    println!("{} 开始扫描...", "▶".cyan());

    // Get scan sources from config
    let mut sources = config.scan_sources.clone();
    // Override max_depth if --depth is specified
    if let Some(d) = depth {
        for source in &mut sources {
            source.max_depth = d;
        }
    }
    if sources.is_empty() {
        anyhow::bail!("没有启用的扫描源");
    }

    // Scan repositories
    let repos = Scanner::scan_all(&sources, &db, true, jobs).await?;

    if repos.is_empty() {
        println!("{} 未找到 Git 仓库", "!".yellow());
        return Ok(());
    }

    let _scan_duration = start.elapsed().as_millis();

    // Optional: fetch all repositories first
    let repos = if should_fetch {
        println!("\n{} 开始 fetch 所有仓库...", "▶".cyan());
        if let Some(proxy) = proxy_config.as_ref() {
            println!("{} 使用代理：{}", "ℹ".blue(), proxy.http_proxy);
        }
        let fetcher = build_scan_fetcher(
            jobs,
            config.default_timeout,
            no_security_check,
            auto_skip_high_risk,
            proxy_config,
        );
        fetcher.fetch_and_rescan(&repos, &db, true).await?
    } else {
        repos
    };

    // Calculate summary
    let mut summary = RepoSummary::new();
    for repo in &repos {
        summary.add(repo);
    }

    // Generate report
    match format {
        OutputFormat::Terminal => {
            let reporter = TerminalReporter::new();
            let report_content = reporter.generate(&repos, &summary)?;
            println!("{}", report_content);
        }
        OutputFormat::Html => {
            let reporter = HtmlReporter::new();
            let report_content = reporter.generate(&repos, &summary)?;
            let extension = reporter.extension();
            let path = save_report_async(report_content, out_path, extension.to_string()).await?;
            println!("{} HTML 报告已保存: {}", "✓".green(), path.display());
        }
        OutputFormat::Markdown => {
            let reporter = MarkdownReporter::new();
            let report_content = reporter.generate(&repos, &summary)?;
            let extension = reporter.extension();
            let path = save_report_async(report_content, out_path, extension.to_string()).await?;
            println!("{} Markdown 报告已保存: {}", "✓".green(), path.display());
        }
    }

    let total_duration = start.elapsed().as_millis();

    // Print summary
    print_scan_summary(&repos, &summary, total_duration);

    Ok(())
}

/// 构造扫描阶段使用的 Fetcher，确保全局网络选项不会在组合命令中丢失。
fn build_scan_fetcher(
    jobs: usize,
    timeout_secs: u64,
    no_security_check: bool,
    auto_skip_high_risk: bool,
    proxy_config: Option<ProxyConfig>,
) -> Fetcher {
    let fetcher = Fetcher::new(jobs, timeout_secs)
        .with_security_scan(!no_security_check)
        .with_auto_skip_high_risk(auto_skip_high_risk);
    match proxy_config {
        Some(proxy) => fetcher.with_proxy(proxy),
        None => fetcher,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_fetch_inherits_global_proxy() {
        let proxy = ProxyConfig {
            enabled: true,
            http_proxy: "http://127.0.0.1:19080".to_string(),
            https_proxy: "http://127.0.0.1:19443".to_string(),
        };

        let fetcher = build_scan_fetcher(5, 47, false, false, Some(proxy));

        assert!(fetcher.configured_proxy().enabled);
        assert_eq!(fetcher.configured_timeout_secs(), 47);
        assert_eq!(
            fetcher.configured_proxy().https_proxy,
            "http://127.0.0.1:19443"
        );
    }
}
