use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "getlatestrepo")]
#[command(about = "快速、优雅的本地 Git 仓库批量管理工具")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// 禁用 pull/reset 前的远程差异安全扫描
    #[arg(long, global = true)]
    pub no_security_check: bool,

    /// 自动跳过高风险仓库（不交互确认，直接跳过）
    #[arg(long, global = true)]
    pub auto_skip_high_risk: bool,

    /// 启用代理（默认：http://127.0.0.1:7890）
    #[arg(long, global = true)]
    pub proxy: bool,

    /// 自定义代理地址（例如：http://127.0.0.1:1080）
    #[arg(long, global = true, value_name = "URL")]
    pub proxy_url: Option<String>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// 初始化配置，添加要管理的目录
    Init {
        /// 根目录路径
        path: PathBuf,
    },

    /// 扫描仓库并生成报告
    Scan {
        /// 扫描前先执行 fetch
        #[arg(long)]
        fetch: bool,

        /// 输出格式
        #[arg(short, long, value_enum, default_value = "terminal")]
        output: OutputFormat,

        /// 输出文件路径（默认自动生成）
        #[arg(short, long)]
        out: Option<PathBuf>,

        /// 限制扫描深度
        #[arg(short, long)]
        depth: Option<usize>,

        /// 并发数（默认：5）
        #[arg(short, long, default_value = "5")]
        jobs: usize,
    },

    /// fetch 所有仓库
    Fetch {
        /// 并发数（默认：5）
        #[arg(short, long, default_value = "5")]
        jobs: usize,

        /// 超时时间，单位秒（默认：30）
        #[arg(short, long, default_value = "30")]
        timeout: u64,
    },

    /// 查看单个仓库详情
    Status {
        /// 仓库路径（设置 --issues 时会忽略）
        path: Option<PathBuf>,

        /// 显示本地变更文件列表
        #[arg(long)]
        diff: bool,

        /// 显示所有异常仓库（needauth、远程不可达、本地修改且落后、路径缺失）
        #[arg(long)]
        issues: bool,
    },

    /// 打开数字菜单式仓库状态控制台
    Tui,

    /// 配置管理
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },

    /// 执行预设工作流
    Workflow {
        /// 工作流名称（daily/check/report/ci/pull-safe/pull-force/pull-backup）
        name: Option<String>,

        /// 列出所有可用工作流
        #[arg(long)]
        list: bool,

        /// 仅显示执行计划，不实际执行
        #[arg(long)]
        dry_run: bool,

        /// 静默模式（仅返回退出码）
        #[arg(long)]
        silent: bool,

        /// 并发数（覆盖工作流默认值）
        #[arg(short, long)]
        jobs: Option<usize>,

        /// 超时时间，单位秒（覆盖工作流默认值）
        #[arg(short, long)]
        timeout: Option<u64>,

        /// pull 后显示新增提交（仅对 pull-safe/pull-force/pull-backup 有效）
        #[arg(long)]
        diff_after: bool,

        /// 自动确认（跳过 Y/n 提示，仅对 pull-safe 有效）
        #[arg(long)]
        yes: bool,

        /// 禁用 pull 安全检查（远程删除检测）
        /// 警告：禁用后，如果远程仓库被清空，可能导致本地代码丢失！
        #[arg(long)]
        no_pull_guard: bool,
    },

    /// 丢弃本地修改
    Discard {
        /// 仓库路径（未提供时显示列表供选择）
        path: Option<String>,

        /// 自动确认（跳过 Y/n 提示）
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub enum ConfigCommands {
    /// 添加扫描路径
    Add { path: PathBuf },

    /// 列出所有配置
    List,

    /// 移除扫描路径
    Remove {
        /// 路径或 ID
        path_or_id: String,
    },

    /// 设置忽略规则（用英文逗号分隔）
    Ignore { patterns: String },

    /// 显示配置文件和数据库位置
    Path,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum OutputFormat {
    /// 终端表格（默认）
    Terminal,
    /// HTML 报告
    Html,
    /// Markdown 报告
    Markdown,
}

impl OutputFormat {
    /// Get file extension for output format
    ///
    /// Currently unused, reserved for future report export functionality
    #[allow(dead_code)]
    pub fn extension(&self) -> &'static str {
        match self {
            OutputFormat::Terminal => "txt",
            OutputFormat::Html => "html",
            OutputFormat::Markdown => "md",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn status_issues_does_not_require_path() {
        let cli = Cli::try_parse_from(["getlatestrepo", "status", "--issues"])
            .expect("status --issues 应允许省略仓库路径");

        match cli.command {
            Commands::Status { path, issues, .. } => {
                assert!(issues);
                assert!(path.is_none());
            }
            _ => panic!("应解析为 status 命令"),
        }
    }
}
