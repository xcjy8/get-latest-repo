#!/bin/sh

# 持久化专项 lane 独立验证临时卷重建、异常清理、TTL 恢复与运行卷保护。
set -eu

# 从脚本目录定位项目根目录与统一生命周期实现。
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROJECT_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
LIFECYCLE="$PROJECT_ROOT/scripts/docker-volume-lifecycle.py"

# 集合门禁开始前先恢复历史过期残留，SIGKILL 资源不会永久堆积。
python3 "$LIFECYCLE" cleanup-expired --scope persistence-test

# 成功路径必须跨容器重建读回 sentinel，并恢复原始卷集合。
python3 "$LIFECYCLE" guard -- python3 "$LIFECYCLE" persistence-test

# 故障路径必须返回非零，并在返回前清理同一 run-id 的全部资源。
set +e
python3 "$LIFECYCLE" guard -- \
  python3 "$LIFECYCLE" persistence-test --intentional-failure
FAILURE_STATUS=$?
set -e
if [ "$FAILURE_STATUS" -eq 0 ]; then
  printf '✗ 持久化测试故障注入未返回非零\n' >&2
  exit 1
fi

# INT/TERM 依赖 finally；SIGKILL 依赖下一次运行的精确 TTL 恢复。
python3 "$LIFECYCLE" signal-cleanup-test --signal INT
python3 "$LIFECYCLE" signal-cleanup-test --signal TERM
python3 "$LIFECYCLE" sigkill-recovery-test

# 删除前必须通过运行卷双重保护，并拒绝未过期、歧义和仍被引用的测试卷。
python3 "$LIFECYCLE" protected-cleanup-test
python3 "$LIFECYCLE" ttl-safety-test

# 最终按项目标签枚举，拒绝容器、网络或卷残留。
python3 "$LIFECYCLE" assert-no-owned-resources --scope persistence-test
printf '✓ Docker volume 持久化专项 lane 验证通过\n'
