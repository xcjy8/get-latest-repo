use anyhow::Result;
use colored::Colorize;
use git2::{Oid, Repository};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

/// Pre-compiled sensitive file patterns (global cache)
static SENSITIVE_PATTERNS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        ".gitignore",
        ".gitmodules",
        "Cargo.toml",
        "package.json",
        "requirements.txt",
        "setup.py",
        "Makefile",
        "Dockerfile",
        ".github/workflows",
        ".gitlab-ci.yml",
        "build.gradle",
        "pom.xml",
        "go.mod",
        // Added: credential and key files
        ".env",
        ".env.local",
        ".env.production",
        ".env.development",
        "*.pem",
        "*.key",
        "id_rsa",
        "id_rsa.pub",
        "id_ed25519",
        "id_ed25519.pub",
        ".aws/credentials",
        ".docker/config.json",
        "kubeconfig",
        "*.p12",
        "*.pfx",
        // Added: CI config files (high risk for supply chain attacks)
        "Jenkinsfile",
        ".circleci/config.yml",
        ".travis.yml",
        "azure-pipelines.yml",
    ]
    .iter()
    .cloned()
    .collect()
});

/// SecurityRisk level
///
/// Note: some levels are currently unused, reserved for future extension
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum RiskLevel {
    /// Safe
    Safe,
    /// Low risk (warning) - currently unused, reserved for future low-sensitivity detection
    Low,
    /// Medium risk (confirmation recommended)
    Medium,
    /// High risk (blocks operation)
    High,
    /// Critical (confirmed danger)
    Critical,
}

impl RiskLevel {
    pub fn emoji(&self) -> &'static str {
        match self {
            RiskLevel::Safe => "✅",
            RiskLevel::Low => "⚡",
            RiskLevel::Medium => "⚠️",
            RiskLevel::High => "🚨",
            RiskLevel::Critical => "☠️",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            RiskLevel::Safe => "安全",
            RiskLevel::Low => "低风险",
            RiskLevel::Medium => "中风险",
            RiskLevel::High => "高风险",
            RiskLevel::Critical => "严重危险",
        }
    }

    pub fn should_block(&self) -> bool {
        matches!(self, RiskLevel::High | RiskLevel::Critical)
    }
}

/// Security risk details
#[derive(Debug, Clone)]
pub struct SecurityRisk {
    /// Risk level
    pub level: RiskLevel,
    /// Risk type
    pub risk_type: RiskType,
    /// Risk description
    pub description: String,
    /// Details (file list, committer, etc.)
    pub details: Vec<String>,
}

/// Risk type
///
/// Note: some types currently unused, reserved for future safety detection extension
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum RiskType {
    /// Remote repository inaccessible (deleted/private) - currently unused, handled by fetch errors
    RemoteInaccessible,
    /// File count anomaly
    FileCountAnomaly,
    /// Sensitive file changes
    SensitiveFileModified,
    /// Committer anomaly
    CommitterAnomaly,
    /// Signature verification failed - currently unused, reserved for future GPG verification
    SignatureVerificationFailed,
}

/// SecurityScan result
#[derive(Debug, Clone)]
pub struct SecurityScanResult {
    /// Is safe
    pub is_safe: bool,
    /// Risk list
    pub risks: Vec<SecurityRisk>,
    /// Highest risk level
    pub max_level: RiskLevel,
}

impl SecurityScanResult {
    /// 是否发现任何风险；中风险虽不阻断操作，也必须向用户展示。
    pub fn has_risks(&self) -> bool {
        !self.risks.is_empty()
    }
}

/// Security scanner
pub struct SecurityScanner;

impl SecurityScanner {
    /// Execute security scan
    ///
    /// Called before fetch/pull, detects safety of remote changes
    pub fn scan_before_fetch(
        path: &Path,
        local_oid: Option<Oid>,
        remote_oid: Option<Oid>,
    ) -> Result<SecurityScanResult> {
        let mut risks = Vec::new();
        let repo = Repository::open(path)?;

        // 1. Check if remote is accessible (requires actual fetch to know, skip here for now)
        // Actually handled by caller on fetch failure

        // 2. If local and remote OIDs exist, analyze differences
        if let (Some(local), Some(remote)) = (local_oid, remote_oid) {
            // 任一底层检查失败都必须中止并上报，禁止把“无法完成扫描”误判为安全。
            let risk = Self::check_file_count_anomaly(&repo, local, remote)?;
            if risk.level != RiskLevel::Safe {
                risks.push(risk);
            }

            let risk = Self::check_sensitive_files(&repo, local, remote)?;
            if risk.level != RiskLevel::Safe {
                risks.push(risk);
            }

            let risk = Self::check_committer_anomaly(&repo, local, remote)?;
            if risk.level != RiskLevel::Safe {
                risks.push(risk);
            }
        }

        let max_level = risks
            .iter()
            .map(|r| r.level)
            .max_by_key(|l| match l {
                RiskLevel::Critical => 4,
                RiskLevel::High => 3,
                RiskLevel::Medium => 2,
                RiskLevel::Low => 1,
                RiskLevel::Safe => 0,
            })
            .unwrap_or(RiskLevel::Safe);

        Ok(SecurityScanResult {
            is_safe: risks.is_empty() || !max_level.should_block(),
            risks,
            max_level,
        })
    }

    /// Detect file count anomaly
    fn check_file_count_anomaly(
        repo: &Repository,
        local: Oid,
        remote: Oid,
    ) -> Result<SecurityRisk> {
        let local_tree = repo.find_commit(local)?.tree()?;
        let remote_tree = repo.find_commit(remote)?.tree()?;

        let local_count = Self::count_files_in_tree(repo, &local_tree)?;
        let remote_count = Self::count_files_in_tree(repo, &remote_tree)?;

        if local_count == 0 {
            return Ok(SecurityRisk {
                level: RiskLevel::Safe,
                risk_type: RiskType::FileCountAnomaly,
                description: String::new(),
                details: Vec::new(),
            });
        }

        let change_ratio = (remote_count as f64 - local_count as f64) / local_count as f64;

        // File decrease > 50% - possible repo deletion
        if change_ratio < -0.5 {
            return Ok(SecurityRisk {
                level: RiskLevel::High,
                risk_type: RiskType::FileCountAnomaly,
                description: format!(
                    "文件数量大幅减少: {} → {}（减少 {:.1}%）",
                    local_count,
                    remote_count,
                    -change_ratio * 100.0
                ),
                details: vec!["⚠️ 远程仓库可能被清空或遭到恶意删除".to_string()],
            });
        }

        // File increase > 200% - possible poisoning (injecting many files)
        if change_ratio > 2.0 {
            return Ok(SecurityRisk {
                level: RiskLevel::Medium,
                risk_type: RiskType::FileCountAnomaly,
                description: format!(
                    "文件数量异常增加: {} → {}（增加 {:.1}%）",
                    local_count,
                    remote_count,
                    change_ratio * 100.0
                ),
                details: vec!["⚠️ 远程新增文件过多，请检查是否包含恶意内容".to_string()],
            });
        }

        Ok(SecurityRisk {
            level: RiskLevel::Safe,
            risk_type: RiskType::FileCountAnomaly,
            description: String::new(),
            details: Vec::new(),
        })
    }

    /// Count files in tree
    fn count_files_in_tree(_repo: &Repository, tree: &git2::Tree) -> Result<usize> {
        let mut count = 0;
        tree.walk(git2::TreeWalkMode::PreOrder, |_, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                count += 1;
            }
            git2::TreeWalkResult::Ok
        })?;
        Ok(count)
    }

    /// Check sensitive file changes
    fn check_sensitive_files(repo: &Repository, local: Oid, remote: Oid) -> Result<SecurityRisk> {
        let local_tree = repo.find_commit(local)?.tree()?;
        let remote_tree = repo.find_commit(remote)?.tree()?;

        let mut modified_sensitive_files = Vec::new();

        // Get diff
        let diff = repo.diff_tree_to_tree(Some(&local_tree), Some(&remote_tree), None)?;

        for delta in diff.deltas() {
            if let Some(path) = delta.new_file().path() {
                let path_str = path.to_string_lossy();
                for pattern in SENSITIVE_PATTERNS.iter() {
                    let matched = if let Some(suffix) = pattern.strip_prefix("*.") {
                        // Glob pattern: match by extension suffix
                        path_str.ends_with(suffix)
                    } else {
                        // 路径组件精确匹配：避免子串误报（如 my-Cargo.toml 不匹配 Cargo.toml）
                        let pattern_components: Vec<&str> = pattern.split('/').collect();
                        let path_components: Vec<&str> = path_str.split('/').collect();
                        pattern_components.len() <= path_components.len()
                            && path_components
                                .windows(pattern_components.len())
                                .any(|window| window == pattern_components.as_slice())
                    };
                    if matched {
                        modified_sensitive_files.push(path_str.to_string());
                        break;
                    }
                }
            }
        }

        if !modified_sensitive_files.is_empty() {
            // Check if credential files or CI configs were modified (critical/high risk)
            let credential_modified = modified_sensitive_files.iter().any(|p| {
                p.contains(".env")
                    || p.ends_with(".pem")
                    || p.ends_with(".key")
                    || p.contains("id_rsa")
                    || p.contains("kubeconfig")
                    || p.contains("credentials")
            });
            let ci_modified = modified_sensitive_files.iter().any(|p| {
                p.contains("workflows") || p.contains("Jenkinsfile") || p.contains(".gitlab-ci")
            });
            let gitignore_modified = modified_sensitive_files
                .iter()
                .any(|p| p.contains(".gitignore"));

            let level = if credential_modified {
                RiskLevel::Critical
            } else if ci_modified || gitignore_modified {
                RiskLevel::High
            } else {
                RiskLevel::Medium
            };

            return Ok(SecurityRisk {
                level,
                risk_type: RiskType::SensitiveFileModified,
                description: format!(
                    "敏感配置文件发生变更: {} 个文件",
                    modified_sensitive_files.len()
                ),
                details: modified_sensitive_files.into_iter().take(5).collect(),
            });
        }

        Ok(SecurityRisk {
            level: RiskLevel::Safe,
            risk_type: RiskType::SensitiveFileModified,
            description: String::new(),
            details: Vec::new(),
        })
    }

    /// Check committer anomalies
    fn check_committer_anomaly(repo: &Repository, local: Oid, remote: Oid) -> Result<SecurityRisk> {
        let mut walk = repo.revwalk()?;
        walk.push(remote)?;
        walk.hide(local)?;

        let mut new_committers: HashMap<String, usize> = HashMap::new();
        let mut unknown_committers = Vec::new();

        // Get known committer list (from local history)
        let known_committers = Self::get_known_committers(repo)?;

        for oid in walk.take(100) {
            let oid = oid?;
            if let Ok(commit) = repo.find_commit(oid) {
                let committer = commit.committer();
                let name = committer.name().unwrap_or("未知").to_string();
                let email = committer.email().unwrap_or("未知").to_string();
                let identity = format!("{} <{}>", name, email);
                *new_committers.entry(identity.clone()).or_insert(0) += 1;

                // Check if it's a new committer (match by name+email combination)
                if !known_committers.contains(&identity) {
                    let commit_id = commit.id().to_string();
                    let short_id = if commit_id.len() >= 7 {
                        &commit_id[..7]
                    } else {
                        &commit_id
                    };
                    unknown_committers.push(format!("{}（提交: {}）", identity, short_id));
                }
            }
        }

        if !unknown_committers.is_empty() {
            return Ok(SecurityRisk {
                level: RiskLevel::Medium,
                risk_type: RiskType::CommitterAnomaly,
                description: format!("发现 {} 个新的未知提交者", unknown_committers.len()),
                details: unknown_committers.into_iter().take(5).collect(),
            });
        }

        Ok(SecurityRisk {
            level: RiskLevel::Safe,
            risk_type: RiskType::CommitterAnomaly,
            description: String::new(),
            details: Vec::new(),
        })
    }

    /// Get known committer list (from local history)
    fn get_known_committers(repo: &Repository) -> Result<HashSet<String>> {
        let mut committers = HashSet::new();
        let mut walk = repo.revwalk()?;

        if let Ok(head) = repo.head()
            && let Some(oid) = head.target()
        {
            walk.push(oid)?;
        }

        for oid in walk.take(200) {
            let oid = oid?;
            if let Ok(commit) = repo.find_commit(oid) {
                let committer = commit.committer();
                let name = committer.name().unwrap_or("未知").to_string();
                let email = committer.email().unwrap_or("未知").to_string();
                committers.insert(format!("{} <{}>", name, email));
            }
        }

        Ok(committers)
    }
}

/// Format security scan result for display
pub fn format_security_report(result: &SecurityScanResult) -> String {
    if !result.has_risks() {
        return format!("{} 安全扫描通过", RiskLevel::Safe.emoji());
    }

    let mut report = String::new();
    report.push_str(&format!("\n{} 安全警告\n", "🛡️".yellow().bold()));
    report.push_str(&format!("{}", "═".repeat(50).yellow()));
    report.push('\n');

    for risk in &result.risks {
        report.push_str(&format!(
            "\n{} {} [{}]\n",
            risk.level.emoji(),
            risk.risk_type_str().red(),
            risk.level.label().yellow()
        ));
        report.push_str(&format!("   {}\n", risk.description));

        if !risk.details.is_empty() {
            report.push_str("   详情:\n");
            for detail in &risk.details {
                report.push_str(&format!("     • {}\n", detail.dimmed()));
            }
        }
    }

    if result.max_level.should_block() {
        report.push('\n');
        report.push_str(&format!(
            "{}",
            "⚠️ 检测到高风险，建议停止操作！\n".red().bold()
        ));
    }

    report
}

impl SecurityRisk {
    fn risk_type_str(&self) -> String {
        match self.risk_type {
            RiskType::RemoteInaccessible => "远程不可访问".to_string(),
            RiskType::FileCountAnomaly => "文件数量异常".to_string(),
            RiskType::SensitiveFileModified => "敏感文件变更".to_string(),
            RiskType::CommitterAnomaly => "提交者异常".to_string(),
            RiskType::SignatureVerificationFailed => "签名验证失败".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit_file(
        repo: &Repository,
        workdir: &Path,
        relative_path: &str,
        content: &str,
        message: &str,
    ) -> Result<Oid> {
        let full_path = workdir.join(relative_path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full_path, content)?;

        let mut index = repo.index()?;
        index.add_path(Path::new(relative_path))?;
        index.write()?;
        let tree_id = index.write_tree()?;
        let tree = repo.find_tree(tree_id)?;
        let signature = git2::Signature::now("GetLatestRepo Test", "test@example.com")?;

        let parent_commits = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .and_then(|oid| repo.find_commit(oid).ok())
            .into_iter()
            .collect::<Vec<_>>();
        let parent_refs = parent_commits.iter().collect::<Vec<_>>();

        let oid = repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parent_refs,
        )?;
        Ok(oid)
    }

    #[test]
    fn test_risk_level_ordering() {
        assert!(!RiskLevel::Safe.should_block());
        assert!(!RiskLevel::Low.should_block());
        assert!(!RiskLevel::Medium.should_block());
        assert!(RiskLevel::High.should_block());
        assert!(RiskLevel::Critical.should_block());
    }

    #[test]
    fn suspicious_code_content_does_not_block_security_scan() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = Repository::init(temp.path())?;
        let local = commit_file(
            &repo,
            temp.path(),
            "src/main.js",
            "export const value = 1;\n",
            "initial safe code",
        )?;
        let remote = commit_file(
            &repo,
            temp.path(),
            "src/main.js",
            "export function run(input) { return eval(input); }\n",
            "add code containing eval",
        )?;

        let result = SecurityScanner::scan_before_fetch(temp.path(), Some(local), Some(remote))?;

        assert!(
            result.is_safe,
            "代码内容模式不应阻塞备份同步，实际风险: {:?}",
            result.risks
        );
        assert!(
            result.risks.is_empty(),
            "eval 等内容模式不应再产生安全扫描风险"
        );
        Ok(())
    }

    #[test]
    fn invalid_commit_oid_fails_closed() -> Result<()> {
        let temp = tempfile::tempdir()?;
        Repository::init(temp.path())?;

        let result = SecurityScanner::scan_before_fetch(
            temp.path(),
            Some(Oid::ZERO_SHA1),
            Some(Oid::ZERO_SHA1),
        );

        assert!(result.is_err(), "无法读取提交时不得返回安全结果");
        Ok(())
    }

    #[test]
    fn medium_risk_is_visible_without_blocking() {
        let result = SecurityScanResult {
            is_safe: true,
            max_level: RiskLevel::Medium,
            risks: vec![SecurityRisk {
                level: RiskLevel::Medium,
                risk_type: RiskType::CommitterAnomaly,
                description: "发现未知提交者".to_string(),
                details: vec!["测试用户 <test@example.com>".to_string()],
            }],
        };

        let report = format_security_report(&result);

        assert!(result.is_safe, "中风险不应自动阻断操作");
        assert!(result.has_risks());
        assert!(report.contains("安全警告"));
        assert!(report.contains("发现未知提交者"));
        assert!(!report.contains("安全扫描通过"));
    }
}
