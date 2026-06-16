//! Scan command handling

use anyhow::Result;
use colored::Colorize;
use std::path::PathBuf;
use std::time::Instant;

use crate::cli::OutputFormat;
use crate::commands::ensure_initialized;
// Database returned by ensure_initialized
use crate::fetcher::Fetcher;
use crate::models::RepoSummary;
use crate::reporter::{
    Reporter, html::HtmlReporter, markdown::MarkdownReporter, save_report_async,
    terminal::TerminalReporter, terminal::print_scan_summary,
};
use crate::scanner::Scanner;

/// Execute scan command
pub async fn execute(
    should_fetch: bool,
    format: OutputFormat,
    out_path: Option<PathBuf>,
    depth: Option<usize>,
    jobs: usize,
    no_security_check: bool,
    auto_skip_high_risk: bool,
) -> Result<()> {
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
        let fetcher = Fetcher::new(jobs, 30)
            .with_security_scan(!no_security_check)
            .with_auto_skip_high_risk(auto_skip_high_risk);
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
