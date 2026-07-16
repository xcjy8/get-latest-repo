pub mod html;
pub mod markdown;
pub mod terminal;

use anyhow::Result;
use std::path::{Path, PathBuf};

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
    if extension.eq_ignore_ascii_case("html") {
        update_latest_html(&path, Path::new("reports"))?;
    }
    Ok(path)
}

/// 将 `reports/latest.html` 原子更新为最新 HTML 报告的绝对路径链接。
///
/// 绝对目标不会受到链接所在目录影响，避免生成 `reports/reports/...` 形式的断链。
fn update_latest_html(report_path: &Path, reports_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(reports_dir)?;
    let latest_link = reports_dir.join("latest.html");
    let absolute_latest = if latest_link.is_absolute() {
        latest_link.clone()
    } else {
        std::env::current_dir()?.join(&latest_link)
    };
    let absolute_report = report_path.canonicalize()?;

    // 用户直接把报告输出到 latest.html 时，它本身就是最新报告，无需链接到自身。
    if absolute_report == absolute_latest {
        return Ok(());
    }

    let temporary_link = reports_dir.join(format!(".latest.html.{}.tmp", std::process::id()));
    if let Err(error) = std::fs::remove_file(&temporary_link)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error.into());
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(&absolute_report, &temporary_link)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&absolute_report, &temporary_link)?;

    #[cfg(windows)]
    if let Err(error) = std::fs::remove_file(&latest_link)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error.into());
    }
    std::fs::rename(&temporary_link, &latest_link)?;
    Ok(())
}

/// Async wrapper for save_report to avoid blocking the async runtime on filesystem I/O
pub async fn save_report_async(
    content: String,
    path: Option<PathBuf>,
    extension: String,
) -> Result<PathBuf> {
    tokio::task::spawn_blocking(move || save_report(&content, path, &extension)).await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(unix)]
    #[test]
    fn html_report_creates_a_resolvable_latest_link() {
        let temp = TempDir::new().unwrap();
        let reports_dir = temp.path().join("reports");
        let report = temp.path().join("archive/report.html");
        std::fs::create_dir_all(report.parent().unwrap()).unwrap();
        std::fs::write(&report, "报告").unwrap();

        update_latest_html(&report, &reports_dir).unwrap();
        let latest = reports_dir.join("latest.html");
        let linked_target = std::fs::read_link(&latest).unwrap();

        assert!(linked_target.is_absolute());
        assert_eq!(linked_target, report.canonicalize().unwrap());
        assert_eq!(std::fs::read_to_string(latest).unwrap(), "报告");
    }
}
