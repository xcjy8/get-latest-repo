#!/usr/bin/env bash
# 使用锁文件构建前端，再将静态资源嵌入 Rust Release 二进制。

set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname -- "$SCRIPT_DIR")"

if ! command -v corepack >/dev/null 2>&1; then
    echo "✗ 缺少 corepack，请安装 Node.js 24.18.0 LTS 或更高版本" >&2
    exit 1
fi

corepack pnpm --dir "$PROJECT_ROOT/frontend" install --frozen-lockfile
corepack pnpm --dir "$PROJECT_ROOT/frontend" build
# rust-toolchain.toml 固定编译器；开发机与 CI 因此使用同一条工具链基线。
cargo build --release --manifest-path "$PROJECT_ROOT/Cargo.toml"
