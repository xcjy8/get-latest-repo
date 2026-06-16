# GetLatestRepo

GetLatestRepo 是一个本地 Git 仓库批量管理工具，适合同时维护几十到几百个仓库的人使用。

它可以递归扫描目录下的 Git 仓库，并发获取远程状态，找出落后、脏工作区、远程不可达、认证异常等问题；也可以按不同策略批量同步仓库，并生成终端、HTML 或 Markdown 报告。

---

## 核心特色

| 能力 | 说明 |
|------|------|
| 批量扫描 | 从一个或多个根目录递归发现 Git 仓库，并记录到本地数据库 |
| 并发 fetch | 同时检查多个仓库远程状态，可设置并发数、超时和代理 |
| 状态汇总 | 展示仓库是否干净、是否落后远程、是否有本地修改、远程是否可访问 |
| 安全拉取 | `pull-safe` 只更新干净仓库，自动跳过有本地修改的仓库 |
| 强制拉取 | `pull-force` 自动 stash 本地修改，再 pull，最后尝试恢复 stash |
| 备份同步 | `pull-backup` 面向纯备份仓库，硬重置到远程状态，并在可能丢失本地历史前创建归档引用 |
| 远程提交留存 | fetch 前后归档远程跟踪分支 HEAD，尽量保留曾经 fetch 到的远程分支提交 |
| 风险扫描 | fetch 前、pull/reset 前检查敏感文件、可疑代码、未知提交者等高风险变化 |
| 认证隔离 | 认证失败或远程不存在的仓库会移动到 `needauth/`，避免反复阻塞正常仓库 |
| 多格式报告 | 支持终端表格、HTML、Markdown，报告按日期归档 |
| 启动自检 | 启动时修复部分路径记录，清理残留临时目录，并在日志中显示当前版本号 |

---

## 基本使用流程

### 1. 添加要管理的目录

```bash
getlatestrepo init ~/projects
```

该命令会把 `~/projects` 加入扫描源。以后执行 scan、fetch、workflow 时，会在这个目录下递归查找 Git 仓库。

### 2. 日常检查所有仓库

```bash
getlatestrepo workflow daily
```

这个工作流会先 fetch，再扫描并在终端输出所有仓库状态。

### 3. 只看需要处理的问题

```bash
getlatestrepo workflow check
```

这个工作流不 fetch，只根据本地数据库和当前工作区状态显示需要关注的仓库。

### 4. 批量同步纯备份仓库

```bash
getlatestrepo workflow pull-backup
```

该模式适合“本地只做镜像备份，不保留手工改动”的仓库集合。它会尽量让本地分支匹配远程跟踪分支。

---

## 全局参数

全局参数可以放在任意子命令前后。

| 参数 | 说明 |
|------|------|
| `--proxy` | 启用默认代理 `http://127.0.0.1:7890` |
| `--proxy-url <URL>` | 使用指定代理，例如 `http://127.0.0.1:1080` |
| `--no-security-check` | 禁用 pull/reset 前的远程差异安全扫描 |
| `--auto-skip-high-risk` | 自动跳过高风险仓库，不进入交互确认 |

示例：

```bash
getlatestrepo --proxy workflow daily
getlatestrepo --proxy-url http://127.0.0.1:1080 fetch
getlatestrepo --auto-skip-high-risk workflow pull-backup
```

---

## 命令一览

| 命令 | 用途 |
|------|------|
| `init <PATH>` | 添加扫描源 |
| `scan` | 扫描仓库并输出报告 |
| `fetch` | 并发 fetch 已记录的仓库 |
| `status [PATH]` | 查看单个仓库状态，或用 `--issues` 查看异常仓库 |
| `config` | 管理扫描源、忽略规则和配置路径 |
| `workflow [NAME]` | 执行内置工作流 |
| `discard [PATH]` | 丢弃本地修改 |

---

## init：添加扫描源

```bash
getlatestrepo init <PATH>
```

示例：

```bash
getlatestrepo init ~/work
getlatestrepo init /Volumes/repos
```

首次使用时建议先执行 `init`。后续可通过 `config add` 添加更多扫描源。

---

## scan：扫描并生成报告

```bash
getlatestrepo scan [OPTIONS]
```

| 参数 | 说明 |
|------|------|
| `--fetch` | 扫描前先 fetch |
| `-o, --output <FORMAT>` | 输出格式：`terminal`、`html`、`markdown` |
| `--out <PATH>` | 指定报告输出路径 |
| `-d, --depth <N>` | 限制递归扫描深度 |
| `-j, --jobs <N>` | 并发数，默认 `5` |

示例：

```bash
# 终端输出扫描结果
getlatestrepo scan

# 先 fetch，再生成 HTML 报告
getlatestrepo scan --fetch --output html

# 只扫描较浅目录，适合目录树很大的情况
getlatestrepo scan --depth 3
```

---

## fetch：并发获取远程状态

```bash
getlatestrepo fetch [OPTIONS]
```

| 参数 | 说明 |
|------|------|
| `-j, --jobs <N>` | 并发数，默认 `5` |
| `-t, --timeout <SECS>` | 单次 fetch 超时秒数，默认 `30` |

示例：

```bash
getlatestrepo fetch
getlatestrepo fetch --jobs 10 --timeout 60
```

fetch 只获取远程对象和远程引用，不会把远程变更合并到工作区。

fetch 会在执行前先归档当前远程跟踪分支 HEAD，fetch 成功后再归档本次新看到的远程跟踪分支 HEAD：

```text
refs/glr-remote-archive/<remote>/<branch>/<timestamp>-<oid>
refs/glr-remote-archive-latest/<remote>/<branch>
```

这样远程以后 force-push、删除分支或删除仓库时，本地曾经 fetch 到且被远程分支指向过的提交仍有引用保护，不会轻易变成可被 Git GC 清理的孤立对象。fetch 前归档保护“上一次已经看到的远程 HEAD”，fetch 后归档保护“这一次刚看到的远程 HEAD”。

---

## status：查看仓库状态

### 查看单个仓库

```bash
getlatestrepo status /path/to/repo
```

显示当前分支、本地修改、ahead/behind、远程地址、最近提交等信息。

### 显示本地变更文件

```bash
getlatestrepo status /path/to/repo --diff
```

### 查看所有异常仓库

```bash
getlatestrepo status --issues
```

`--issues` 会汇总以下问题：

- 认证隔离仓库
- 远程不可达仓库
- 本地有修改且落后远程的仓库
- 数据库记录存在但本地路径缺失的仓库

---

## config：配置管理

```bash
getlatestrepo config <SUBCOMMAND>
```

| 子命令 | 说明 |
|--------|------|
| `add <PATH>` | 添加扫描源 |
| `list` | 列出当前配置 |
| `remove <PATH_OR_ID>` | 按路径或 ID 移除扫描源 |
| `ignore <PATTERNS>` | 设置忽略规则，多个规则用英文逗号分隔 |
| `path` | 显示配置文件和数据库位置 |

示例：

```bash
getlatestrepo config add ~/more-repos
getlatestrepo config list
getlatestrepo config ignore 'node_modules,target,vendor,.cache'
getlatestrepo config path
```

忽略规则会影响后续扫描，也会同步到已有扫描源配置。

---

## workflow：内置工作流

```bash
getlatestrepo workflow [NAME] [OPTIONS]
```

### 可用工作流

| 工作流 | 流程 | 适用场景 |
|--------|------|----------|
| `daily` | fetch → scan | 日常巡检所有仓库 |
| `check` | scan | 快速查看需要关注的仓库 |
| `report` | fetch → scan(HTML) | 生成完整 HTML 报告 |
| `ci` | fetch → scan → check | 检查是否存在落后远程的仓库 |
| `pull-safe` | fetch → scan → pull | 只更新干净仓库 |
| `pull-force` | fetch → scan → stash → pull → pop | 自动保存本地修改后再更新 |
| `pull-backup` | fetch → scan → stash → hard reset → pop | 纯备份镜像同步 |

### workflow 参数

| 参数 | 说明 |
|------|------|
| `--list` | 列出所有工作流 |
| `--dry-run` | 只显示执行计划，不实际执行 |
| `--silent` | 静默模式，主要用于脚本判断退出码 |
| `-j, --jobs <N>` | 覆盖工作流默认并发数 |
| `-t, --timeout <SECS>` | 覆盖工作流默认超时 |
| `--diff-after` | pull 后显示本次新增提交 |
| `--yes` | 跳过 `pull-safe` 的确认提示 |
| `--no-pull-guard` | 禁用 `pull-safe` 的远程删除保护 |

### 常用示例

```bash
# 日常检查
getlatestrepo workflow daily

# 查看工作流计划
getlatestrepo workflow pull-backup --dry-run

# 生成 HTML 报告
getlatestrepo workflow report

# 批量安全拉取，只处理干净仓库
getlatestrepo workflow pull-safe --yes --diff-after

# 本地有修改也尝试更新，失败时保留 stash 供手动恢复
getlatestrepo workflow pull-force --diff-after

# 纯备份同步，适合本地不保留手工改动的镜像仓库
getlatestrepo workflow pull-backup --jobs 10 --timeout 60

# 用于脚本：有仓库落后远程时返回非零退出码
getlatestrepo workflow ci --silent
```

---

## 三种 Pull 策略

### pull-safe：保守更新

```bash
getlatestrepo workflow pull-safe
```

行为：

- 只更新工作区干净的仓库
- 有本地修改的仓库会跳过
- 默认使用 fast-forward only，避免自动产生 merge commit
- 适合包含本地长期改动仓库的目录

### pull-force：保存本地修改后更新

```bash
getlatestrepo workflow pull-force
```

行为：

- 有本地修改时先创建 stash
- 执行 fast-forward pull
- pull 成功后尝试 stash pop
- stash pop 冲突时停止，并提示 stash 名称和恢复命令

适合希望批量同步，但仍可能保留本地临时修改的仓库。

### pull-backup：严格镜像远程

```bash
getlatestrepo workflow pull-backup
```

行为：

- fetch 后对落后仓库执行备份同步
- 如果本地 HEAD 会被 hard reset 丢弃，先创建归档引用
- 归档引用位置为 `refs/glr-archive/history/<branch>/<timestamp>` 和 `refs/glr-archive/latest/<branch>`
- 本地有修改时会先 stash，再 hard reset 到远程跟踪分支，最后尝试 stash pop
- 如果检测到未合并索引，会跳过 stash 并按备份模式硬重置恢复
- 如果遇到超长 symlink 检出问题，会尝试以 `core.symlinks=false` 回退

适合纯备份仓库。不要把它用于需要长期保留本地手工改动的仓库。

---

## 安全扫描和高风险确认

GetLatestRepo 会在 fetch 前做安全预扫描，也会在 pull/reset 前比较本地 `HEAD` 与 upstream tracking ref，检查真实远程差异。

### 会检查什么

| 类型 | 说明 |
|------|------|
| 敏感文件变更 | `.env`、密钥文件、CI 配置、容器配置、运行配置等 |
| 可疑代码模式 | `eval`、`exec`、`system`、`subprocess`、`child_process`、base64 解码、`curl | sh` 等 |
| 提交者异常 | 新增未知提交者 |
| 文件数量异常 | 文件数量大幅减少或异常增加 |

### 高风险如何确认

如果多个仓库命中高风险，工具会一次性列出所有风险仓库和原因，然后等待一次输入。fetch 阶段和 pull/reset 阶段都使用同一套批量选择方式：

```text
输入 0 表示全部继续；
输入 1,3,5 表示只继续这些序号；
直接回车表示全部跳过。
```

继续的仓库会执行本次 fetch、pull 或 reset；未选择的仓库会保持当前本地状态。

### 跳过或关闭

```bash
# 自动跳过高风险仓库，不进入交互
getlatestrepo --auto-skip-high-risk workflow pull-backup

# 完全关闭 pull/reset 前安全扫描
getlatestrepo --no-security-check workflow pull-backup
```

不建议长期关闭安全扫描。

---

## 认证隔离 needauth

当 fetch 返回认证失败、授权失败或远程仓库不存在时，GetLatestRepo 会把仓库移动到扫描根目录下的 `needauth/`。

这样做的目的：

- 不让需要登录或已删除的仓库反复阻塞正常仓库
- 保留本地仓库内容，不直接删除
- 后续认证恢复后，可以从 `needauth/` 迁回原位置

典型路径：

```text
<扫描根目录>/needauth/<repo-name>
```

如果只是 DNS、代理、超时等临时网络问题，工具会归类为网络错误，不会移动到 `needauth/`。

---

## discard：丢弃本地修改

```bash
getlatestrepo discard [PATH] [OPTIONS]
```

| 参数 | 说明 |
|------|------|
| `PATH` | 指定仓库路径；不传时列出可处理仓库供选择 |
| `--yes` | 跳过确认提示 |

示例：

```bash
getlatestrepo discard /path/to/repo
getlatestrepo discard --yes
```

该命令会丢弃本地未提交修改。执行前请确认这些修改不需要保留。

---

## 报告

报告默认保存到：

```text
reports/YYYY/MM/DD/getlatestrepo-report-YYYYMMDD-HHMMSS.<ext>
```

支持格式：

| 格式 | 用法 |
|------|------|
| 终端表格 | `getlatestrepo scan --output terminal` |
| HTML | `getlatestrepo scan --output html` 或 `getlatestrepo workflow report` |
| Markdown | `getlatestrepo scan --output markdown` |

`reports/latest.html` 会指向最新 HTML 报告，方便快速打开最近一次结果。

---

## 配置和数据位置

默认位置：

| 内容 | 路径 |
|------|------|
| 配置文件 | `~/.config/getlatestrepo/config.toml` |
| 数据库 | `~/.config/getlatestrepo/getlatestrepo.db` |
| 进程锁 | `~/.cache/getlatestrepo.lock` |
| 报告目录 | 当前项目目录下的 `reports/` |

可通过命令查看实际路径：

```bash
getlatestrepo config path
```

可用环境变量：

| 变量 | 说明 |
|------|------|
| `GETLATESTREPO_CONFIG_DIR` | 覆盖配置和数据库目录 |
| `HTTP_PROXY` / `HTTPS_PROXY` | 系统代理环境变量 |

---

## 常见问题

### 扫描太慢怎么办？

使用较小的扫描深度：

```bash
getlatestrepo scan --depth 3
```

也可以配置忽略规则：

```bash
getlatestrepo config ignore 'node_modules,target,vendor,.cache'
```

### 为什么有些仓库被移动到 needauth？

通常是认证失败、权限不足或远程仓库不存在。仓库只是被隔离到 `needauth/`，本地内容不会被直接删除。

### 远程仓库删除了，本地会被删除吗？

不会因为远程仓库 404 就删除本地仓库。fetch 会失败并进入认证隔离流程，本地 `.git` 和已有提交仍在。

### pull-backup 会不会丢提交？

如果 hard reset 会丢弃当前本地 HEAD，工具会先创建 `refs/glr-archive/` 归档引用。已经存在于本地对象库里的提交通常可以通过归档引用找回。

另外，每次 fetch 前后，工具都会把当前可见的远程跟踪分支 HEAD 归档到 `refs/glr-remote-archive/`。这能进一步保护“曾经 fetch 到且当时被远程分支指向过”的提交。

注意：如果某些远程提交从未 fetch 到本地，工具无法凭空恢复这些提交。

### 高风险扫描提示很多，怎么快速处理？

高风险仓库会批量汇总显示。输入：

- `0`：全部继续
- `1,3,5`：只继续指定序号
- 直接回车：全部跳过

如果希望无人值守执行，可以使用：

```bash
getlatestrepo --auto-skip-high-risk workflow pull-backup
```

### 如何确认本地命令是不是最新版？

执行：

```bash
getlatestrepo --version
```

启动自检日志也会显示版本号，例如：

```text
ℹ️  启动自检完成（v0.1.11），已修复 1 条记录
```

---

## 许可证

本项目采用双许可模式：

- AGPL-3.0-or-later：用于开源和非商业用途
- 商业许可：用于闭源或商业用途

如需在闭源或商业产品中使用，请联系版权所有者获取商业许可。
