#!/bin/sh
# GetLatestRepo 唯一打包入口。
#
# 设计目标：
# - 必须支持：sh /Users/sy/local-dev-workbench/sp-pro-ai/get-latest-repo/scripts/package.sh
# - 每次都先构建 release，确保部署的是最新源码对应的二进制。
# - 只复制 release 产物，不移动 target/release/getlatestrepo，方便后续继续验证。
# - 固定安装到 /Users/sy/local-bin/custom-getlatestrepo，并创建 getrep 入口。
# - 修正 ~/.binquick 里旧的 getrep workflow alias，避免 getrep --version 被 alias 展开成 workflow 参数。

set -eu

CLR_RESET="$(printf '\033[0m')"
CLR_GREEN="$(printf '\033[0;32m')"
CLR_RED="$(printf '\033[0;31m')"
CLR_YELLOW="$(printf '\033[0;33m')"
CLR_BLUE="$(printf '\033[0;34m')"
CLR_CYAN="$(printf '\033[0;36m')"
CLR_DIM="$(printf '\033[2m')"

SCRIPT_DIR="$(CDPATH= cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(CDPATH= cd "${SCRIPT_DIR}/.." && pwd)"
SOURCE_BIN="${PROJECT_ROOT}/target/release/getlatestrepo"

TARGET_DIR="${GETLATESTREPO_TARGET_DIR:-/Users/sy/local-bin}"
TARGET_BIN="${GETLATESTREPO_TARGET_BIN:-${TARGET_DIR}/custom-getlatestrepo}"
GETREP_DIR="${GETLATESTREPO_GETREP_DIR:-/Users/sy/.local/bin}"
GETREP_BIN="${GETLATESTREPO_GETREP_BIN:-${GETREP_DIR}/getrep}"
BINQUICK_FILE="${GETLATESTREPO_BINQUICK_FILE:-/Users/sy/.binquick}"
BACKUP_KEEP="${GETLATESTREPO_BACKUP_KEEP:-5}"

START_TIME="$(date +%s)"
BACKUP_FILE=""

step_start() {
    printf '\n%s├─ [%s/%s] %s%s\n' "$CLR_CYAN" "$1" "$2" "$3" "$CLR_RESET"
}

step_detail() {
    printf '%s│  %s %s%s%s: %s%s\n' "$CLR_BLUE" "$1" "$2" "$CLR_RESET" "$CLR_DIM" "$3" "$CLR_RESET"
}

step_ok() {
    printf '%s│  %s✓%s %s%s%s\n' "$CLR_BLUE" "$CLR_GREEN" "$CLR_RESET" "$CLR_GREEN" "$1" "$CLR_RESET"
}

step_warn() {
    printf '%s│  %s⚠%s %s%s%s\n' "$CLR_BLUE" "$CLR_YELLOW" "$CLR_RESET" "$CLR_YELLOW" "$1" "$CLR_RESET"
}

step_err() {
    printf '%s│  %s✗%s %s%s%s\n' "$CLR_BLUE" "$CLR_RED" "$CLR_RESET" "$CLR_RED" "$1" "$CLR_RESET"
}

die() {
    step_err "$1"
    printf '%s✗ 打包失败%s\n' "$CLR_RED" "$CLR_RESET"
    exit 1
}

format_duration() {
    secs="$1"
    if [ "$secs" -lt 60 ]; then
        printf '%ss' "$secs"
    else
        printf '%sm %ss' "$((secs / 60))" "$((secs % 60))"
    fi
}

file_size() {
    if command -v stat >/dev/null 2>&1; then
        stat -f '%z bytes' "$1" 2>/dev/null && return 0
        stat -c '%s bytes' "$1" 2>/dev/null && return 0
    fi
    wc -c <"$1" | awk '{print $1 " bytes"}'
}

cargo_version() {
    awk -F '"' '/^version[[:space:]]*=/ { print $2; exit }' "${PROJECT_ROOT}/Cargo.toml"
}

quote_for_single_alias() {
    # 当前部署路径不包含单引号；这里仍保留校验，避免生成无法 source 的 alias。
    case "$1" in
        *"'"*) return 1 ;;
        *) printf "%s" "$1" ;;
    esac
}

fix_binquick_alias() {
    [ -n "$BINQUICK_FILE" ] || return 0

    alias_target="$(quote_for_single_alias "$TARGET_BIN")" || {
        step_warn "部署路径包含单引号，跳过 ~/.binquick alias 修复"
        return 0
    }
    alias_line="alias getrep='${alias_target}'"

    if [ ! -e "$BINQUICK_FILE" ]; then
        step_detail "🧩" "快捷文件" "创建 ${BINQUICK_FILE}"
        {
            printf '# GetLatestRepo 快捷入口\n'
            printf '%s\n' "$alias_line"
        } >"$BINQUICK_FILE" || die "无法创建 ${BINQUICK_FILE}"
        chmod 600 "$BINQUICK_FILE" 2>/dev/null || true
        step_ok "已创建 getrep alias"
        return 0
    fi

    if [ ! -f "$BINQUICK_FILE" ]; then
        step_warn "${BINQUICK_FILE} 不是普通文件，跳过 alias 修复"
        return 0
    fi

    stamp="$(date +%Y%m%d-%H%M%S)"
    binquick_backup="${BINQUICK_FILE}.bak.${stamp}"
    cp "$BINQUICK_FILE" "$binquick_backup" 2>/dev/null || die "无法备份 ${BINQUICK_FILE}"

    tmp_file="${BINQUICK_FILE}.tmp.$$"
    awk -v alias_line="$alias_line" '
        BEGIN { replaced = 0 }
        /^[[:space:]]*alias[[:space:]]+getrep=/ {
            if (replaced == 0) {
                print alias_line
                replaced = 1
            }
            next
        }
        { print }
        END {
            if (replaced == 0) {
                print ""
                print "# --- getlatestrepo ---"
                print alias_line
            }
        }
    ' "$BINQUICK_FILE" >"$tmp_file" || {
        rm -f "$tmp_file"
        die "无法更新 ${BINQUICK_FILE}"
    }

    mv "$tmp_file" "$BINQUICK_FILE" || die "无法替换 ${BINQUICK_FILE}"
    chmod 600 "$BINQUICK_FILE" 2>/dev/null || true
    step_detail "🧩" "快捷入口" "${BINQUICK_FILE}: ${alias_line}"
    step_detail "💾" "快捷备份" "$binquick_backup"
    step_ok "已修正 getrep alias"
}

cleanup_old_backups() {
    [ "$BACKUP_KEEP" -ge 0 ] 2>/dev/null || BACKUP_KEEP=5
    [ -d "$TARGET_DIR" ] || return 0

    # macOS 默认 find 没有 -printf；这里用 ls -t 做保留最近 N 个的兼容清理。
    set +e
    old_list="$(ls -t "${TARGET_BIN}.bak."* 2>/dev/null | awk -v keep="$BACKUP_KEEP" 'NR > keep { print }')"
    set -e
    [ -n "$old_list" ] || return 0

    printf '%s\n' "$old_list" | while IFS= read -r old_backup; do
        [ -n "$old_backup" ] && rm -f "$old_backup" 2>/dev/null || true
    done
    step_ok "已清理旧二进制备份"
}

printf '\n%s╔══════════════════════════════════════════════════╗%s\n' "$CLR_CYAN" "$CLR_RESET"
printf '%s║%s     %sGetLatestRepo 打包部署脚本%s              %s║%s\n' "$CLR_CYAN" "$CLR_RESET" "$CLR_YELLOW" "$CLR_RESET" "$CLR_CYAN" "$CLR_RESET"
printf '%s╚══════════════════════════════════════════════════╝%s\n\n' "$CLR_CYAN" "$CLR_RESET"

step_start 1 6 "构建最新 Release 二进制"
step_detail "📁" "项目目录" "$PROJECT_ROOT"
step_detail "🔧" "构建命令" "cargo build --release"
cd "$PROJECT_ROOT"
cargo build --release || die "cargo build --release 失败"
[ -f "$SOURCE_BIN" ] || die "找不到构建产物 ${SOURCE_BIN}"
FILE_SIZE="$(file_size "$SOURCE_BIN")"
VERSION_INFO="v$(cargo_version)"
step_detail "📦" "构建产物" "${SOURCE_BIN} (${FILE_SIZE})"
step_ok "构建完成"

step_start 2 6 "准备安装目录"
mkdir -p "$TARGET_DIR" "$GETREP_DIR" || die "无法创建安装目录"
step_detail "📁" "二进制目录" "$TARGET_DIR"
step_detail "📁" "getrep 目录" "$GETREP_DIR"
step_ok "目录就绪"

step_start 3 6 "备份旧二进制"
if [ -f "$TARGET_BIN" ]; then
    stamp="$(date +%Y%m%d-%H%M%S)"
    BACKUP_FILE="${TARGET_BIN}.bak.${stamp}"
    cp "$TARGET_BIN" "$BACKUP_FILE" || die "无法备份旧二进制"
    step_detail "💾" "备份文件" "$BACKUP_FILE"
    step_ok "备份完成"
else
    step_detail "📋" "旧二进制" "不存在，无需备份"
    step_ok "跳过备份"
fi

step_start 4 6 "安装最新二进制"
tmp_target="${TARGET_BIN}.tmp.$$"
cp "$SOURCE_BIN" "$tmp_target" || die "复制新二进制失败"
chmod +x "$tmp_target" || die "设置执行权限失败"
mv "$tmp_target" "$TARGET_BIN" || die "替换目标二进制失败"
step_detail "🎯" "部署路径" "$TARGET_BIN"
step_ok "二进制已安装"

step_start 5 6 "创建 getrep 命令入口"
ln -sfn "$TARGET_BIN" "$GETREP_BIN" || die "创建 getrep 软链接失败"
step_detail "🔗" "软链接" "${GETREP_BIN} -> ${TARGET_BIN}"
fix_binquick_alias
step_ok "getrep 入口已就绪"

step_start 6 6 "验证安装结果"
version_output="$("$TARGET_BIN" --version 2>&1)" || die "目标二进制 --version 验证失败"
printf '%s\n' "$version_output" | grep 'getlatestrepo' >/dev/null || die "版本输出不符合预期: ${version_output}"
help_output="$("$GETREP_BIN" --help 2>&1)" || die "getrep --help 验证失败"
printf '%s\n' "$help_output" | grep 'tui' >/dev/null || die "getrep --help 未包含 tui 命令"
step_detail "📋" "版本输出" "$version_output"
step_detail "📋" "TUI 检查" "getrep --help 包含 tui"
cleanup_old_backups
step_ok "验证通过"

END_TIME="$(date +%s)"
ELAPSED="$((END_TIME - START_TIME))"

printf '\n%s└─%s %s打包完成%s\n' "$CLR_CYAN" "$CLR_RESET" "$CLR_GREEN" "$CLR_RESET"
printf '   %s├─%s %s版本%s: %s%s%s\n' "$CLR_BLUE" "$CLR_RESET" "$CLR_DIM" "$CLR_RESET" "$CLR_CYAN" "$VERSION_INFO" "$CLR_RESET"
printf '   %s├─%s %s二进制%s: %s%s%s\n' "$CLR_BLUE" "$CLR_RESET" "$CLR_DIM" "$CLR_RESET" "$CLR_CYAN" "$TARGET_BIN" "$CLR_RESET"
printf '   %s├─%s %s命令入口%s: %s%s%s\n' "$CLR_BLUE" "$CLR_RESET" "$CLR_DIM" "$CLR_RESET" "$CLR_CYAN" "$GETREP_BIN" "$CLR_RESET"
printf '   %s├─%s %s备份文件%s: %s%s%s\n' "$CLR_BLUE" "$CLR_RESET" "$CLR_DIM" "$CLR_RESET" "$CLR_CYAN" "${BACKUP_FILE:-无}" "$CLR_RESET"
printf '   %s└─%s %s耗时%s: %s%s%s\n\n' "$CLR_BLUE" "$CLR_RESET" "$CLR_DIM" "$CLR_RESET" "$CLR_CYAN" "$(format_duration "$ELAPSED")" "$CLR_RESET"

printf '%s当前终端如果已经加载过旧 alias，请执行：source %s%s\n' "$CLR_YELLOW" "$BINQUICK_FILE" "$CLR_RESET"
printf '%s之后固定使用：getrep --version / getrep tui / getrep workflow pull-backup%s\n' "$CLR_GREEN" "$CLR_RESET"
