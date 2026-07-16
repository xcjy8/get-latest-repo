#!/bin/sh

# 一键串行执行两个独立 lane；CI 会把二者拆成不同 job。
set -eu

# 从脚本目录定位项目，避免依赖调用者当前工作目录。
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROJECT_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

# 普通 lane 永远先执行；它不创建持久卷，也不依赖持久化专项 lane。
"$PROJECT_ROOT/scripts/verify-docker-volumes-ordinary.sh"

# 持久化 lane 使用独立入口，避免普通测试为验证持久化而放宽零卷合同。
"$PROJECT_ROOT/scripts/verify-docker-volumes-persistence.sh"

printf '✓ Docker volume 完整生命周期治理验证通过\n'
