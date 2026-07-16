#!/usr/bin/env bash
# 从任意工作目录构建并安装 GetLatestRepo。

set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname -- "$SCRIPT_DIR")"
INSTALL_DIR="${GETLATESTREPO_INSTALL_DIR:-/usr/local/bin}"
SOURCE_BIN="$PROJECT_ROOT/target/release/getlatestrepo"
TARGET_BIN="$INSTALL_DIR/getlatestrepo"

echo "=========================================="
echo "GetLatestRepo 安装程序"
echo "=========================================="
echo "检测到系统：$(uname -s) $(uname -m)"

# 安装目标必须对应当前源码；即使已有旧二进制，也重新构建前端与 Rust Release。
echo "正在构建当前源码的完整 Release……"
"$PROJECT_ROOT/scripts/build-all.sh"

# 只有目标目录或文件不可写时才请求管理员权限，便于自定义目录无 sudo 安装。
if [[ -d "$INSTALL_DIR" && -w "$INSTALL_DIR" ]]; then
    install -m 0755 "$SOURCE_BIN" "$TARGET_BIN"
else
    if ! command -v sudo >/dev/null 2>&1; then
        echo "✗ 安装目录不可写且系统没有 sudo：$INSTALL_DIR" >&2
        exit 1
    fi
    echo "安装到 $INSTALL_DIR 需要管理员权限"
    sudo install -d -m 0755 "$INSTALL_DIR"
    sudo install -m 0755 "$SOURCE_BIN" "$TARGET_BIN"
fi

echo "✓ 安装完成：$TARGET_BIN"
echo "版本信息："
"$TARGET_BIN" --version
echo ""
echo "常用命令："
echo "  getlatestrepo init <路径>"
echo "  getlatestrepo workflow daily"
echo "  getlatestrepo --help"
