//! Workflow command handling

use anyhow::Result;
use colored::Colorize;

use crate::git::ProxyConfig;
use crate::workflow::{BuiltInWorkflows, WorkflowExecutor, list_workflows};

/// Execute workflow command
#[allow(clippy::too_many_arguments)]
pub async fn execute(
    name: Option<String>,
    list: bool,
    dry_run: bool,
    silent: bool,
    jobs: Option<usize>,
    timeout: Option<u64>,
    diff_after: bool,
    yes: bool,
    no_security_check: bool,
    auto_skip_high_risk: bool,
    no_pull_guard: bool,
    proxy_config: Option<ProxyConfig>,
) -> Result<i32> {
    // List workflows
    if list {
        list_workflows();
        return Ok(0);
    }

    // Must have name
    let name = match name {
        Some(n) => n,
        None => {
            eprintln!("{} 请指定工作流名称", "✗".red());
            eprintln!("\n运行 `getlatestrepo workflow --list` 查看可用工作流");
            anyhow::bail!("工作流名称不能为空");
        }
    };

    // Get workflow
    let workflow = match BuiltInWorkflows::get(&name) {
        Some(w) => w,
        None => {
            eprintln!("{} 未知工作流: {}", "✗".red(), name);
            eprintln!("\n运行 `getlatestrepo workflow --list` 查看可用工作流");
            anyhow::bail!("未知工作流: {}", name);
        }
    };

    // Modify workflow steps based on parameters
    let mut workflow = workflow;
    for step in &mut workflow.steps {
        match step {
            crate::workflow::WorkflowStep::PullSafe {
                diff_after: da,
                confirm,
                ..
            } => {
                *da = diff_after;
                if yes {
                    *confirm = false;
                }
            }
            crate::workflow::WorkflowStep::PullForce { diff_after: da, .. } => {
                *da = diff_after;
            }
            crate::workflow::WorkflowStep::PullBackup { diff_after: da, .. } => {
                *da = diff_after;
            }
            _ => {}
        }
    }

    // Execute workflow
    let mut executor = WorkflowExecutor::new(workflow, jobs, timeout, dry_run, silent)
        .with_security_check(!no_security_check)
        .with_auto_skip_high_risk(auto_skip_high_risk)
        .with_pull_safety_check(!no_pull_guard); // Enabled repo-deletion detection by default

    if let Some(proxy) = proxy_config
        && proxy.enabled
    {
        executor = executor.with_proxy(proxy);
    }

    let result = executor.execute().await?;

    // Return appropriate exit code
    Ok(result.exit_code())
}
