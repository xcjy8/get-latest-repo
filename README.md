<div align="center">

# GetLatestRepo

[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange?logo=rust)](https://www.rust-lang.org)
[![CI](https://github.com/xcjy8/GetLatestRepo/actions/workflows/ci.yml/badge.svg)](https://github.com/xcjy8/GetLatestRepo/actions)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)

**Rust 编写的高性能本地 Git 仓库批量管理工具**

[English](README.en.md)

</div>

---

## 为什么需要 GetLatestRepo？

如果你同时维护着几十甚至上百个 Git 仓库，以下场景一定不陌生：

- 每天早上逐个 `cd` 进目录、`git fetch`、`git pull`，重复操作浪费时间
- 不确定哪些仓库有本地未提交的修改，贸然 pull 可能引发冲突
- 想要一份全局的仓库状态报告，却只能手动汇总
- CI/CD 流水线需要检测所有仓库是否与远程同步，缺少现成工具

**GetLatestRepo 就是为了解决这些问题而生的。** 一条命令扫描所有仓库、并发 fetch、安全拉取、生成可视化报告，把重复劳动变成自动化工作流。

---

## 核心功能

| 能力 | 说明 |
|------|------|
| **递归扫描** | 秒级发现指定目录下所有 Git 仓库，SQLite 缓存避免重复扫描 |
| **并发 Fetch** | 基于 Tokio 异步并发，可配置并发数与超时，支持代理 |
| **安全拉取** | `pull-safe` 自动跳过有本地修改的仓库；`pull-force` 自动 stash → pull → pop |
| **备份同步** | `pull-backup` 硬重置到远程最新状态，适合纯备份场景 |
| **安全扫描** | fetch 后、pull 前检查远程差异：检测敏感文件变更、可疑代码模式、未知提交者 |
| **工作流引擎** | 内置 7 种工作流，串联 fetch → scan → pull → report 全流程 |
| **多格式报告** | 终端表格 / HTML（暗色主题）/ Markdown，自动按日期归档 |
| **进程锁** | 防止多实例并发运行导致数据竞争 |

---

## 截图

<p align="center">
  <img src="docs/images/01.jpg" alt="终端表格报告" width="80%">
</p>

<p align="center">
  <img src="docs/images/02.jpg" alt="HTML 暗色主题报告" width="80%">
</p>

<p align="center">
  <img src="docs/images/03.jpg" alt="工作流执行" width="80%">
</p>

---

## 安装

### 从源码编译

```bash
git clone https://github.com/xcjy8/GetLatestRepo.git
cd GetLatestRepo
cargo build --release

# 可选：安装到系统路径
sudo cp target/release/getlatestrepo /usr/local/bin/
```

### 环境要求

- Rust 1.85+（使用 Rust Edition 2024）
- Git（系统已安装）

### 从 Release 下载

前往 [GitHub Releases](https://github.com/xcjy8/GetLatestRepo/releases) 下载预编译二进制。

---

## 快速开始

```bash
# 1. 添加扫描目录
getlatestrepo init ~/projects

# 2. 运行日常工作流（fetch + scan + 状态汇总）
getlatestrepo workflow daily

# 3. 生成 HTML 报告并自动打开浏览器
getlatestrepo workflow report
```

三步即可完成：添加目录 → 执行工作流 → 查看报告。

---

## 命令详解

### 全局参数

| 参数 | 说明 |
|------|------|
| `--proxy` | 启用默认代理 `http://127.0.0.1:7890` |
| `--proxy-url <URL>` | 指定自定义代理地址 |
| `--no-security-check` | 禁用预操作安全扫描 |

### 命令一览

| 命令 | 说明 |
|------|------|
| `init <path>` | 添加扫描目录 |
| `scan` | 递归扫描所有 Git 仓库 |
| `fetch` | 并发 fetch 所有仓库 |
| `status <path>` | 查看单个仓库详细状态 |
| `config` | 管理扫描源、忽略规则、配置 |
| `workflow <name>` | 执行工作流 |
| `discard` | 交互式丢弃本地修改 |

### `init`

```bash
getlatestrepo init <PATH>
```

将指定目录添加为扫描源，后续 scan/fetch 操作会递归发现该目录下的所有 Git 仓库。

### `scan`

```bash
getlatestrepo scan [OPTIONS]
```

| 参数 | 说明 |
|------|------|
| `--fetch` | 扫描前先执行 fetch |
| `-o, --output <FORMAT>` | 输出格式：`terminal`（默认）、`html`、`markdown` |
| `--out <PATH>` | 自定义输出文件路径 |
| `-d, --depth <N>` | 限制扫描深度 |
| `-j, --jobs <N>` | 并发数（默认 5） |

### `fetch`

```bash
getlatestrepo fetch [OPTIONS]
```

| 参数 | 说明 |
|------|------|
| `-j, --jobs <N>` | 并发数（默认 5） |
| `-t, --timeout <SECS>` | 单次 fetch 超时秒数（默认 30） |

### `status`

```bash
getlatestrepo status <PATH> [OPTIONS]
```

| 参数 | 说明 |
|------|------|
| `--diff` | 显示 diff 内容 |

### `config`

```bash
getlatestrepo config <SUBCOMMAND>
```

| 子命令 | 说明 |
|--------|------|
| `add <PATH>` | 添加扫描源 |
| `list` | 列出所有扫描源 |
| `remove <PATH_OR_ID>` | 移除扫描源 |
| `ignore <PATTERNS>` | 设置全局忽略规则（逗号分隔） |
| `path` | 显示配置文件路径 |

### `workflow`

```bash
getlatestrepo workflow [NAME] [OPTIONS]
```

| 参数 | 说明 |
|------|------|
| `--list` | 列出所有可用工作流 |
| `--dry-run` | 只显示执行计划，不实际运行 |
| `--silent` | 静默模式（仅返回退出码） |
| `-j, --jobs <N>` | 覆盖默认并发数 |
| `-t, --timeout <SECS>` | 覆盖默认超时 |
| `--yes` | 自动确认提示（仅 `pull-safe`） |
| `--diff-after` | pull 后显示新提交（仅 pull 类工作流） |
| `--no-pull-guard` | 禁用拉取安全检查（仅 `pull-safe`） |

### `discard`

```bash
getlatestrepo discard [PATH] [OPTIONS]
```

| 参数 | 说明 |
|------|------|
| `--yes` | 跳过确认提示 |

---

## 工作流引擎

工作流是 GetLatestRepo 的核心设计。每个工作流由多个步骤串联执行，覆盖从 fetch 到报告的完整流程。

### 内置工作流

| 工作流 | 步骤 | 说明 |
|--------|------|------|
| `daily` | fetch → scan | 日常巡检：拉取最新状态，终端展示汇总 |
| `check` | scan（仅扫描） | 快速查看：不 fetch，只显示需要关注的仓库 |
| `report` | fetch → scan（HTML） | 生成完整 HTML 报告，自动打开浏览器 |
| `ci` | fetch → scan → check | CI 检查：有落后的仓库时返回错误码 |
| `pull-safe` | fetch → scan → pull | 安全拉取：跳过有本地修改的仓库 |
| `pull-force` | fetch → scan → stash → pull → pop | 强制拉取：自动 stash 本地修改 |
| `pull-backup` | fetch → scan → hard reset | 备份同步：硬重置到远程状态，适合纯备份 |

### 使用示例

```bash
# 日常巡检
getlatestrepo workflow daily

# CI 流水线集成（失败返回非零退出码）
getlatestrepo workflow ci --silent

# 安全批量拉取（自动跳过 dirty 仓库）
getlatestrepo workflow pull-safe --yes --diff-after

# 生成报告（自定义并发和超时）
getlatestrepo workflow report --jobs 10 --timeout 60

# 查看执行计划（不实际运行）
getlatestrepo workflow pull-force --dry-run
```

---

## 安全扫描机制

GetLatestRepo 会先执行 fetch 下载远程对象，但不会合并到工作区；随后在 pull/reset 前比较本地 `HEAD` 与 upstream tracking ref，检查真实远程差异：

### 检测项

| 类别 | 检测内容 |
|------|----------|
| **敏感文件变更** | `.env`、密钥文件（`.pem`、`id_rsa`）、CI 配置（`.github/workflows`、`Jenkinsfile`）、容器凭证（`.docker/config.json`、`kubeconfig`） |
| **可疑代码模式** | `eval()`/`exec()` 调用、base64 解码、暗网地址、`curl \| sh`、`wget` 下载等 |
| **未知提交者** | 不在已知贡献者列表中的新提交者 |

### 风险等级

| 等级 | 处理方式 |
|------|----------|
| Safe | 正常执行 |
| Medium | 提示确认 |
| High | 阻断操作 |

可通过 `--no-security-check` 全局参数禁用 pull 前安全扫描。

---

## 配置文件

配置文件位于 `~/.config/getlatestrepo/config.toml`（可通过 `getlatestrepo config path` 查看）。

### 默认配置

```toml
default_jobs = 5        # 默认并发数
default_timeout = 30    # 默认超时（秒）
default_depth = 5       # 默认扫描深度

# 忽略规则
ignore_patterns = [
    ".git",
    "node_modules",
    "target",
    "vendor",
    ".idea",
    ".vscode",
]

# 同步配置
[sync]
auto_sync = true        # 自动扫描新增仓库
strict_sync = false     # 严格模式：数量不一致时全量扫描
```

### 环境变量

| 变量 | 说明 |
|------|------|
| `GETLATESTREPO_CONFIG_DIR` | 覆盖配置目录路径 |
| `HTTP_PROXY` / `HTTPS_PROXY` | 系统代理（也可用 `--proxy` 参数） |

---

## 报告系统

生成的报告自动归档到：

```
reports/YYYY/MM/DD/getlatestrepo-report-YYYYMMDD-HHMMSS.<ext>
```

- `reports/latest.html` 符号链接始终指向最新的 HTML 报告
- 支持终端表格、HTML（暗色主题）、Markdown 三种格式
- HTML 报告支持自动打开浏览器

---

## 技术栈

| 组件 | 技术选型 |
|------|----------|
| CLI 框架 | clap 4.5 |
| Git 操作 | libgit2（git2 crate） |
| 异步运行时 | Tokio |
| 数据库 | SQLite（rusqlite + WAL 模式） |
| 终端输出 | comfy-table + colored + indicatif（进度条） |
| HTML 模板 | Askama |
| 配置格式 | TOML + JSON |

### 构建优化

Release 构建启用 LTO、单代码生成单元、符号剥离，确保二进制体积最小化且运行高效。

---

## 常见问题

### Q: 扫描速度慢怎么办？

使用 `-d` 参数限制扫描深度，或在 `config.toml` 中调整 `default_depth`。对于大型目录树，适当减小深度可以显著提升速度。

### Q: 如何排除特定目录？

通过 `getlatestrepo config ignore <patterns>` 设置忽略规则，支持逗号分隔的多个模式。该命令会同步更新现有扫描源和未来新增扫描源。默认已忽略 `node_modules`、`target`、`vendor` 等常见目录。

### Q: `pull-safe` 和 `pull-force` 有什么区别？

- `pull-safe`：只拉取干净的仓库（无本地修改），有修改的仓库会被跳过
- `pull-force`：自动 stash 本地修改 → pull → stash pop，适合批量同步但可能产生冲突
- `pull-backup`：面向纯备份仓库，fetch 后硬重置到远程跟踪分支；如果本地已有未合并索引、空 stash 状态或超长 symlink 检出问题，会尽量自动恢复并给出明确诊断

### Q: 如何在 CI 中使用？

```bash
getlatestrepo workflow ci --silent
if [ $? -ne 0 ]; then
    echo "有仓库落后于远程"
    exit 1
fi
```

### Q: 代理不生效？

优先级：`--proxy-url` > `--proxy` > 系统环境变量 `HTTP_PROXY`/`HTTPS_PROXY`。确保代理地址正确且可访问。

---

## 贡献指南

欢迎提交 Issue 和 Pull Request！

1. Fork 本仓库
2. 创建特性分支：`git checkout -b feature/your-feature`
3. 提交更改：`git commit -m "feat: add your feature"`
4. 推送分支：`git push origin feature/your-feature`
5. 创建 Pull Request

### 开发环境

```bash
git clone https://github.com/xcjy8/GetLatestRepo.git
cd GetLatestRepo
cargo build
cargo test
```

---

## 版本日志

| 版本 | 主要变更 |
|------|----------|
| v0.1.9 | 依赖安全升级、自动同步修复与发布前质量加固 |
| v0.1.8 | pull-backup 工作流与 README 中英文重构 |
| v0.1.7 | 全量缺陷修复与安全加固 |
| v0.1.6 | Rust Edition 2024、死代码清理、预扫描安全批次 |
| v0.1.5 | 移除 git2 网络 fetch 路径，基于数据验证优化 |
| v0.1.4 | fetch 双层架构、进度条精简与 git2 偏好缓存 |
| v0.1.3 | 三层优雅关闭 + 启动自检 + 残留清理 |
| v0.1.2 | 14 项缺陷修复（安全/并发/Git 状态/信号/阻塞 IO） |
| v0.1.1 | P0/P1/P2 全量修复与安全重构 |
| v0.1.0 | 初始发布 |

完整变更记录见 [GitHub Releases](https://github.com/xcjy8/GetLatestRepo/releases)。

---

## 许可证

本项目采用双许可模式：

- **AGPL-3.0-or-later** — 用于开源和非商业用途，详见 [LICENSE](LICENSE)
- **商业许可** — 用于闭源或商业用途，请联系作者获取商业许可

如需在商业产品中使用本软件且不公开源代码，须从版权所有者处获取单独的商业许可。

---

## 作者

**xcjy8** — [GitHub](https://github.com/xcjy8)

项目地址：[https://github.com/xcjy8/GetLatestRepo](https://github.com/xcjy8/GetLatestRepo)
