//! General utility functions

use std::path::Path;

// ==================== Constants ====================

/// Directory name for repositories requiring authentication
pub const NEEDAUTH_DIR: &str = "needauth";

/// Default HTTP proxy address
pub const DEFAULT_PROXY_URL: &str = "http://127.0.0.1:7890";

/// Default max concurrency for scanning
pub const DEFAULT_MAX_CONCURRENT_SCAN: usize = 8;

/// Per-recv timeout for concurrent execution (seconds)
pub const CONCURRENT_RECV_TIMEOUT_SECS: u64 = 30;

/// Sanitize URL, remove credential info
///
/// Convert `https://token@github.com/user/repo.git` to `https://github.com/user/repo.git`
pub fn sanitize_url(url: &str) -> String {
    // Parse URL, remove user info part
    if let Ok(parsed) = url::Url::parse(url)
        && (parsed.username() != "" || parsed.password().is_some())
    {
        // Rebuild URL without credentials
        let mut cleaned = parsed.clone();
        cleaned.set_username("").ok();
        cleaned.set_password(None).ok();
        return cleaned.to_string();
    }
    // If parsing fails, return original URL (may be local path or other format)
    url.to_string()
}

/// Sanitize path, only show last two directory levels
///
/// Examples:
/// - `/home/user/projects/myrepo` -> `.../projects/myrepo`
/// - `myrepo` -> `myrepo`
pub fn sanitize_path(path: &str) -> String {
    let path = Path::new(path);
    let components: Vec<_> = path.components().collect();

    if components.len() <= 2 {
        // Path is short, return directly
        path.to_string_lossy().to_string()
    } else {
        // Only show last two levels
        let last_two: Vec<_> = components.iter().rev().take(2).rev().collect();
        // Safety check: ensure there are two elements (avoid panic)
        if last_two.len() < 2 {
            path.to_string_lossy().to_string()
        } else {
            format!(
                ".../{}/{}",
                last_two[0].as_os_str().to_string_lossy(),
                last_two[1].as_os_str().to_string_lossy()
            )
        }
    }
}

/// Sanitize path (Path version)
#[allow(dead_code)]
pub fn sanitize_path_buf(path: &Path) -> String {
    sanitize_path(&path.to_string_lossy())
}

/// Check if a directory entry should be ignored based on patterns
///
/// Pattern rules:
/// - Exact match: `node_modules` matches directory named exactly `node_modules`
/// - Prefix match: `target*` matches any directory starting with `target`
/// - Wildcard all: `*` or `**` matches everything
pub fn should_ignore_entry(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| {
        if p == "*" || p == "**" {
            return true;
        }
        if let Some(prefix) = p.strip_suffix('*') {
            // Prefix match (e.g. "target*" → match "target", "target-temp")
            if prefix.is_empty() {
                return true;
            }
            name.starts_with(prefix)
        } else {
            // Exact match
            name == p
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_short_path() {
        assert_eq!(sanitize_path("myrepo"), "myrepo");
        assert_eq!(sanitize_path("/myrepo"), "/myrepo");
    }

    #[test]
    fn test_sanitize_long_path() {
        assert_eq!(
            sanitize_path("/home/user/projects/myrepo"),
            ".../projects/myrepo"
        );
        assert_eq!(sanitize_path("/home/user/spgit/myrepo"), ".../spgit/myrepo");
    }

    #[test]
    fn test_sanitize_url_with_credentials() {
        assert_eq!(
            sanitize_url("https://token@github.com/user/repo.git"),
            "https://github.com/user/repo.git"
        );
        assert_eq!(
            sanitize_url("https://user:pass@github.com/user/repo.git"),
            "https://github.com/user/repo.git"
        );
    }

    #[test]
    fn test_sanitize_url_without_credentials() {
        assert_eq!(
            sanitize_url("https://github.com/user/repo.git"),
            "https://github.com/user/repo.git"
        );
    }

    #[test]
    fn test_sanitize_url_invalid() {
        // Invalid URL should be returned as-is
        assert_eq!(sanitize_url("not-a-url"), "not-a-url");
    }

    #[test]
    fn test_should_ignore_entry_exact() {
        assert!(should_ignore_entry(
            "node_modules",
            &["node_modules".to_string()]
        ));
        assert!(!should_ignore_entry(
            "my_node_modules",
            &["node_modules".to_string()]
        ));
    }

    #[test]
    fn test_should_ignore_entry_prefix() {
        assert!(should_ignore_entry("target", &["target*".to_string()]));
        assert!(should_ignore_entry("target-temp", &["target*".to_string()]));
        assert!(!should_ignore_entry("my-target", &["target*".to_string()]));
    }

    #[test]
    fn test_should_ignore_entry_wildcard() {
        assert!(should_ignore_entry("anything", &["*".to_string()]));
        assert!(should_ignore_entry("anything", &["**".to_string()]));
    }

    #[test]
    fn test_should_ignore_entry_no_match() {
        assert!(!should_ignore_entry(
            "src",
            &["node_modules".to_string(), "target*".to_string()]
        ));
    }
}
