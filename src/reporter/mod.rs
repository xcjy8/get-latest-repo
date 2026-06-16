pub mod html;
pub mod markdown;
pub mod terminal;

use anyhow::Result;
use std::path::PathBuf;

use crate::models::{RepoSummary, Repository};

/// Reporter trait
pub trait Reporter {
    /// Generate report
    fn generate(&self, repos: &[Repository], summary: &RepoSummary) -> Result<String>;

    /// File extension
    fn extension(&self) -> &'static str;
}

/// Save report to file
/// Defaults to saving in the reports/YYYY/MM/DD/ directory
pub fn save_report(content: &str, path: Option<PathBuf>, extension: &str) -> Result<PathBuf> {
    let path = match path {
        Some(p) => p,
        None => {
            let now = chrono::Local::now();
            let timestamp = now.format("%Y%m%d-%H%M%S");

            // Build reports/YYYY/MM/DD/ path
            let report_dir = PathBuf::from("reports")
                .join(now.format("%Y").to_string())
                .join(now.format("%m").to_string())
                .join(now.format("%d").to_string());

            // Ensure directory exists
            std::fs::create_dir_all(&report_dir)?;

            report_dir.join(format!("getlatestrepo-report-{}.{}", timestamp, extension))
        }
    };

    std::fs::write(&path, content)?;
    Ok(path)
}

/// Async wrapper for save_report to avoid blocking the async runtime on filesystem I/O
pub async fn save_report_async(
    content: String,
    path: Option<PathBuf>,
    extension: String,
) -> Result<PathBuf> {
    tokio::task::spawn_blocking(move || save_report(&content, path, &extension)).await?
}
