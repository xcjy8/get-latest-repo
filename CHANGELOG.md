# Changelog

---

## [0.1.11] - 2026-06-16

> 高风险批量确认、远程提交长期留存、版本确认与中文 README 用户手册重写

### Changed

- **高风险确认体验**: Pull 前安全扫描命中多个高风险仓库时，改为一次性列出全部风险并通过序号批量选择，`0` 表示全部继续，回车表示全部跳过。
- **Fetch 高风险确认体验**: fetch 前安全预扫描也改为批量确认，避免大量高风险仓库逐个询问。
- **启动版本提示**: 启动自检日志追加当前二进制版本号，方便确认 `getrep` alias 是否已经覆盖到最新版。
- **README 简体中文重写**: 移除英文 README、截图、开发与贡献内容，改为面向使用者的完整中文功能和命令手册。

### Fixed

- **基础命令可用性**: `--help` / `--version` 先于进程锁执行，避免缓存目录不可写或已有实例运行时无法确认版本。
- **远程提交长期留存**: fetch 前后都会归档远程跟踪分支 HEAD 到 `refs/glr-remote-archive/`，尽量保留曾经 fetch 到且被远程分支引用过的提交。
- **备份归档命名加固**: `pull-backup` 本地 HEAD 归档改为 `refs/glr-archive/history/<branch>/<timestamp>` 和 `refs/glr-archive/latest/<branch>`，兼容包含 `/` 的分支名。

### Tests

- **回归测试补强**: 增加 `--version` 早退出和 fetch 前后远程 HEAD 归档的真实流程测试。

---

## [0.1.10] - 2026-06-16

> 批量仓库同步可靠性加固、项目级 Rust Skill 与 pull-backup 恢复路径完善

### Fixed

- **Pull-backup 异常恢复**: 修复未合并索引、空 stash、超长 symlink 检出导致的备份同步失败，并补强错误诊断与回退路径。
- **仓库批量管理稳定性**: 修复并发执行器卡死、DNS 错误误判、`status --issues` 参数、配置/数据库同步、HTML 状态样式与发布脚本隔离问题。

### Changed

- **项目级 Rust Skill**: 新增 `.claude/skills/getlatestrepo-rust/`，沉淀本仓库 Rust/Git/SQLite/安全扫描/验证工作流规范。
- **安全扫描语义更新**: 文档明确安全扫描在 fetch 后、pull/reset 前执行，以真实远程差异作为检查对象。

---

## [0.1.9] - 2026-05-11

> 依赖安全升级、自动同步修复与发布前质量加固

### Changed

- **依赖安全升级**: `git2` 升级至 0.20，`indicatif` 升级至 0.18，并刷新锁定依赖。
- **Rust 版本基线统一**: 明确最低 Rust 版本为 1.85+，与 Rust Edition 2024 保持一致。
- **中文输出完善**: CLI、工作流、报告、安全扫描、数据库与错误信息继续统一为中文表达。
- **集成测试增强**: `scripts/test-all.sh` 增加 `status --issues` 与 `workflow pull-backup --dry-run` 覆盖。

### Fixed

- **fetch 自动同步开关**: `fetch` 和工作流执行器现在遵循配置中的 `sync.auto_sync`，不再绕过用户设置。
- **首次 fetch 空数据库体验**: 数据库为空时自动执行一次扫描，避免首次使用陷入“请先扫描”的循环提示。
- **Clippy 兼容性**: 修正 `list_archive_refs` 中的迭代写法，确保严格 lint 下无警告。

---

## [0.1.8] - 2026-05-05

> pull-backup 工作流与 README 中英文重构

### Changed

- **新增 pull-backup 工作流**: 提供先备份再拉取的工作流路径，降低批量更新时的恢复成本。
- **README 中英文重构**: 优化用户文档结构与双语说明，提升安装、使用和工作流理解效率。

---

## [0.1.7] - 2026-05-05

> 全量缺陷修复与安全加固（20 项 P0/P1/Warn 级别修复）

### Fixed

- **P0-1 checkout_tree 静默跳过**: `pull_ff_only` 增加 SAFE checkout 二次验证 + hard-reset 补刀，失败回滚 ref
- **P0-2 panic 空记录**: panic 分支不再使用 `Repository::default()`，保留原始 repo 信息
- **P1-3 硬编码 "origin"**: 新增 `GitOps::get_remote_name()` 从上游跟踪引用动态解析远程名
- **P1-4 auto_skip_high_risk 语义反向**: 修正为 `true` 时自动扫描并跳过（不交互），新增 CLI `--auto-skip-high-risk`
- **P1-5 报告格式不一致**: `only_dirty_or_behind=true` 时 Terminal/HTML/Markdown 统一使用过滤后的 `report_repos`
- **P1-6 diff_after 显示错误**: 新增 `GitOps::get_commits_since(original_oid)`，pull 前记录原始 OID，成功后精确显示新增提交
- **P1-7 needauth 重命名误判删除**: `cleanup_deleted_repos` 通过 `.needauth_original_path` sidecar 文件定位重命名仓库
- **P1-8 跨文件系统移动失败**: 新增 `Fetcher::move_or_copy_dir()`，`EXDEV` 回退到 `cp -a` (Unix) / `robocopy /MOVE` (Windows)
- **P1-9 敏感文件匹配过于宽松**: `path_str.contains(pattern)` 改为路径组件精确匹配
- **P1-10 Windows PID 文件锁 TOCTOU**: `Drop` 仅当 PID 匹配才删除；stale lock 3 次重试；`is_process_running` 增加进程名校验
- **Warn-11 spawn_blocking 超时**: `inspect`、`check_pull_safety`、`get_commits_since` 全部包装 `tokio::time::timeout(30s)`
- **Warn-12 unchecked_transaction**: rusqlite 升级至 0.32，全部替换为 `immediate_transaction()`
- **Warn-13 git 时区偏移**: `get_last_commit_info` 使用 `time.offset_minutes()` 正确构造 `FixedOffset`
- **Warn-14 忙等+线程泄漏**: `concurrent.rs` 添加 join deadline 防止 hang
- **Warn-16 pull_force 冲突后 DB 未刷新**: `execute_pull_force` 结束后对冲突仓库也执行 `inspect` + `upsert_repository`
- **Warn-17 ReDoS**: `SUSPICIOUS_PATTERNS` 中所有 `.*` / `\s*` 改为有限量词（`\s{0,20}` 等）
- **Warn-18 路径遍历 Unicode 绕过**: 移除 `contains("..")` 前置检查，完全依赖 `canonicalize` + `starts_with`

### Changed

- **config remove 路径 canonicalize**: `config remove` 时对输入路径先 `canonicalize`，匹配 `config add` 存储格式
- **配置目录环境变量支持**: `config_dir()` 优先读取 `GETLATESTREPO_CONFIG_DIR`，便于测试隔离
- **集成测试环境隔离**: `scripts/test-all.sh` 使用临时 `GETLATESTREPO_CONFIG_DIR`，在测试目录内初始化真实 Git 仓库

---

## [0.1.6] - 2026-05-05

> Rust Edition 2024 升级、死代码清理与安全扫描前置

### Changed

- **Rust Edition 2024**: `Cargo.toml` 升级至 `edition = "2024"`
- **标准库替代 once_cell**: `security.rs` 中 `once_cell::sync::Lazy` 全部迁移至 `std::sync::LazyLock`，移除 `once_cell` 依赖
- **清理死代码与预留字段**:
  - `fetcher.rs`: 移除 `fallback_from_git2`、`fallback_reason` 等已废弃字段及相关方法
  - `git.rs`: 移除 `fetch_with_git2`、`new`、`set_proxy` 等未使用接口
  - `fetcher.rs`: 移除 `with_auto_skip_high_risk` 未使用构建器方法
- **语法升级**: 大量 `if let` 嵌套改写为 Rust 2024 `if let ... &&` 链式语法（`scanner.rs`、`security.rs`、`fetcher.rs` 等）
- **scanner 并发参数化**: `scan()` 新增 `jobs` 参数，取消硬编码 `DEFAULT_MAX_CONCURRENT`，改为运行时 `clamp(1, 100)`
- **Windows 进程锁改进**: `main.rs` 中 PID 文件使用 `create_new` 原子创建，新增过期锁检测与自动恢复机制
- **reporter/terminal**: 精简终端报告头部输出，优化 Summary 排版

### Added

- **fetcher 安全扫描前置批处理**: 新增 `prescan_security_batch()`，在 fetch 前并发执行安全扫描，高风险仓库汇总后统一交互确认

### Removed

- 删除 `src/network_test.rs` 网络诊断测试模块（未在生产中使用）

---

## [0.1.5] - 2026-04-22

> 基于数据验证结果，彻底移除 git2 网络 fetch 路径

### Changed

- `git.rs`: `fetch_detailed` 直接走 `fetch_with_git_command`，不再尝试 git2
- `fetcher.rs`: 移除 git2 fallback 缓存、fallback 信息汇总等配套逻辑
- `git.rs`: 保留 `fetch_with_git2` 作为预留接口（标记 `#[allow(dead_code)]`），本地操作未来仍可能使用

### Fixed

- 修复双层架构导致的每个仓库浪费 3 秒等待问题（git2 平均 1600ms > 原生 git 1200ms，且无优势）
- 修复 5 并发下 git2 部分仓库耗时翻倍的问题（如 JustAnime 888ms→3491ms）

---

## [0.1.4] - 2026-04-22

> fetch 双层架构、进度条精简与 git2 偏好缓存

### Added

**fetch 双层架构（git2 快速路径 + 原生 git 命令兜底）**

- `git.rs`: `fetch_detailed` 改为三层策略
  1. 若仓库已在 `git2_fallback_cache` 中，直接走原生 `git fetch`
  2. 否则启动 git2 fetch，3 秒超时监控
  3. git2 失败/超时时 fallback 到 `fetch_with_git_command`
- `git.rs`: 新增 `fetch_with_git_command`，使用 `std::process::Command` 执行 `git fetch origin`
  - 支持 `child.kill()` 强制终止，避免 git2 无法中断的问题
  - 代理通过环境变量 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 传递，兼容旧版本 git
  - 设置 `GIT_TERMINAL_PROMPT=0` 防止交互式阻塞
- `git.rs`: `FetchStatus` 统一错误分类（`classify_error`），兼容 git2 和原生 git 的错误文本

**git2 偏好缓存**

- `fetcher.rs`: `Fetcher` 新增 `git2_fallback_cache: Arc<Mutex<HashSet<String>>>`
- 只要某仓库发生过 git2 → git 命令的 fallback，路径即写入缓存
- 下次 fetch 同一仓库时直接跳过 git2，避免重复浪费 3 秒超时等待
- 缓存为进程级（随 `Fetcher` 实例生命周期），不持久化到数据库

### Changed

**进度条与输出排版**

- `fetcher.rs`: 去掉 `MultiProgress`，改用单个 `ProgressBar`
- `fetcher.rs`: fetch 过程中不再穿插任何 `pb.println` 输出，进度条保持干净
- `fetcher.rs`: 所有 fallback / 失败 / 移动 / 恢复信息在进度条 `finish_and_clear()` 后统一树形输出
- `fetcher.rs`: fallback 汇总指明具体仓库名和原始原因（如 `"git2 fetch 3s 内未返回"`）

**进程优雅关闭**

- `main.rs`: 末尾从 `Ok(ExitCode::from(exit_code))` 改为直接 `std::process::exit(exit_code)`
- `signal_handler.rs`: 补充注释，说明后台 `tokio::spawn` 的 `ctrl_c()` 监听任务会导致 tokio runtime 在 main 返回后无法退出

**代理兼容性**

- `git.rs`: 原生 git 命令路径不再使用 `git -c http.proxy=`（旧版本 git 不支持），改用环境变量传递代理

### Fixed

- `fetcher.rs`: 修复 `fallback_reason` 在重试时可能丢失的问题，始终保留第一次的原始 fallback 原因
- `fetcher.rs`: 修复缓存策略过保守的问题，fallback 发生后即写入缓存（不限于成功时）
- `workflow/executor.rs`: 修复 pull-force 冲突恢复命令的树形输出格式

---

## [0.1.3] - 2026-04-21

> package.sh 新增打包脚本

### Added


**优雅关闭（Graceful Shutdown）三层策略**

解决 1000 仓库场景下 Ctrl+C 无法停止的核心痛点：

1. **密集检查点** — `fetcher.rs` 结果收集循环改用 `timeout(200ms)` 轮询，检测到 shutdown 立即 break；`fetch_and_rescan` repo 循环、`concurrent.rs` 线程创建循环、`workflow/executor.rs` Pull 安全检查循环均加入 `is_shutdown_requested()` 检查
2. **main 末尾自动 exit** — 命令执行返回后若 shutdown 标志已设置，直接 `process::exit(0)`，不等 tokio runtime 等待后台 `spawn_blocking` 线程
3. **10 秒兜底 + 双按 Ctrl+C** — `signal_handler.rs` 第一次 Ctrl+C 设标志并启动 10 秒定时器；第二次 Ctrl+C 或 10 秒超时均立即 `process::exit(130)`

**启动自检（Startup Cleanup）**

- `main.rs` 新增 `run_startup_cleanup()`：打开数据库后遍历所有记录
  - 若记录路径不存在但 `needauth/` 下有同名仓库 → 自动修复路径
  - 若路径不存在且 needauth 下也没有 → 删除孤儿记录
  - 遍历所有 `scan_sources` 的 `needauth/` 目录，清理 `.getlatestrepo_swap` 残留临时目录
- 自检仅在非 `init` 命令时执行，避免初始化前误操作

### Changed

- `signal_handler.rs`: 重写为三层关闭策略，原 `AtomicBool` 单标志升级为 `tokio::select!` 竞争模型
- `fetcher.rs`: `fetch_all_detailed` 结果收集从裸 `futures.next().await` 改为 `timeout` 轮询
- `concurrent.rs`: 线程创建循环增加 shutdown 检查，剩余任务直接发 `None`

---

## [0.1.2] - 2026-04-21

### Fixed

> 14 项缺陷修复（安全/并发/Git 状态/信号/阻塞 IO）

**安全 (Critical):**

- `fetcher`: `move_repo_to_needauth` / `move_repo_from_needauth` 新增 `expected_parent` 参数，拒绝绝对路径遍历攻击
- `fetcher`: 回滚恢复失败不再静默忽略，返回 CRITICAL 错误并告知用户临时路径位置

**并发/异步 (High):**

- `fetcher`: 所有 `spawn_blocking`（scan/move/inspect/fetch）包裹 `timeout`，防止 Semaphore 泄漏导致软死锁
- `fetcher`: 重试总时间限制为 `timeout_secs * 2`，避免指数退火导致超时失控
- `fetcher`: `fetch_and_update` DB 循环移至 `spawn_blocking`，避免阻塞异步运行时
- `reporter`: `save_report` 新增 `save_report_async`，文件写入不再阻塞 async 线程
- `status`: `--issues` 的 `db.list_repositories()` 包裹 `spawn_blocking`
- `concurrent`: 线程栈大小降至 1MB，减少被遗弃线程的内存泄漏

**Git 状态 (High):**

- `git`: `pull_ff_only` / `pull_force` 在 `set_target` 失败后自动回滚 `checkout_tree` 到原始提交，防止工作目录与 HEAD 不一致
- `git`: `pull_force` pull 失败后若 stash 已创建，主动警告用户 stash 名称和恢复命令，避免孤儿 stash
- `git`: `find_stash_index` / `get_conflict_files` 错误分支改为显式警告，不再静默吞掉错误

**其他修复 (Medium):**

- `models`/`scanner`/`config`/`db`/`sync`: `max_depth` 类型从 `i32` 改为 `usize`，消除负值 round-trip 导致深度限制失效的 bug
- `signal_handler`/`workflow`/`fetcher`: 移除 `#[allow(dead_code)]`，`SHUTDOWN_REQUESTED` 在 workflow 步骤循环和 fetch future 生成循环中实际生效
- `workflow/executor`: 两处 `let _ =`（`ensure_reports_dir`、`upsert_repository`）改为显式错误日志

### Notes

- `fetcher`: 恢复路径存在设计限制——假设原始仓库是扫描根的直接子目录。若原始路径为嵌套目录，恢复后位置可能不正确。完整修复需 DB schema 变更以保存原始相对路径。
- `reporter`/`scanner`: `Path::exists()` 为阻塞文件系统 I/O，已在代码中添加注释说明。

---

## [0.1.1] - 2026-04-21

### Refactor

> P0/P1/P2 全量修复与安全重构

P0 修复:

- scanner/sync: 修复 *.txt glob 匹配导致目录被全部过滤的 bug
- commands/scan: --depth 参数正确传递至 Scanner
- main/workflow/signal: 移除 process::exit，确保 flock 文件锁正常释放
- fetcher/scanner/executor: 所有 git2/fs 阻塞操作移至 spawn_blocking
- concurrent: 实现真实任务级超时（超时后放弃线程，避免永久阻塞）
- fetcher/scanner: needauth 移动后保留 DB 记录，cleanup 不再误删

P1 改进:

- db: 文件权限 0o600、WAL + synchronous=NORMAL + temp_store=MEMORY
- db/models: dirty_files 从换行分隔迁移至 JSON 数组（向后兼容解析）
- security: once_cell::Lazy 缓存敏感模式集，扩展 .env/.pem/CI 等检测
- 全仓库: 修正错别字、统一输出图标、中文格式化时长
- git/reporter: upstream_url 和路径脱敏，防止敏感信息泄漏
- utils: 提取 NEEDAUTH_DIR/DEFAULT_PROXY_URL 等共享常量
- fetcher/executor: TTY 检测避免非交互环境 stdin 挂起

P2 清理:

- workflow/executor: 提取 RepoChangeView trait + print_repo_change_tree，
消除 execute() 与 execute_pull_safe() 中 ~150 行脏仓库树形渲染重复

---

## [0.1.0] - 2026-04-09

### Added

- 🎉 Initial release of GetLatestRepo.
