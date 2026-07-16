#!/bin/sh

# 该入口只创建 manifest 中缺失的运行卷；不会删除、重建或重命名任何现有卷。
set -eu

# 从脚本目录稳定定位项目根目录，允许用户在任意工作目录执行。
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROJECT_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

# 所有精确名称、用途和标签命名空间都由版本化 manifest 提供。
exec python3 "$PROJECT_ROOT/scripts/docker-volume-lifecycle.py" \
  --manifest "$PROJECT_ROOT/docker/volumes.manifest.json" \
  provision
