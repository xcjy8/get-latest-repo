#!/bin/sh

# 普通测试 lane 只验证 tmpfs 零卷合同，和持久化专项测试保持独立入口。
set -eu

# 从脚本目录定位项目根目录与统一生命周期实现。
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROJECT_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
LIFECYCLE="$PROJECT_ROOT/scripts/docker-volume-lifecycle.py"

# 静态合同必须先通过，避免在错误 Compose 模型上启动测试容器。
python3 "$LIFECYCLE" audit-compose
python3 "$LIFECYCLE" cleanup-expired --scope ordinary-test

# 成功路径必须保持 Docker volume 全局集合不变。
python3 "$LIFECYCLE" guard -- "$PROJECT_ROOT/scripts/docker-test-ordinary.sh"

# 故障路径必须返回非零，但同样先由集合守卫证明没有卷增减。
set +e
python3 "$LIFECYCLE" guard -- \
  "$PROJECT_ROOT/scripts/docker-test-ordinary.sh" --intentional-failure
FAILURE_STATUS=$?
set -e
if [ "$FAILURE_STATUS" -eq 0 ]; then
  printf '✗ 普通测试故障注入未返回非零\n' >&2
  exit 1
fi

# 最终按项目标签枚举，拒绝容器、网络或卷残留。
python3 "$LIFECYCLE" assert-no-owned-resources --scope ordinary-test
printf '✓ Docker volume 普通零卷 lane 验证通过\n'
