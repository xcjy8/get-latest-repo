#!/bin/sh

# 普通测试使用 tmpfs 覆盖镜像数据目录，并证明测试前后 Docker volume 全局集合不变。
set -eu

# 从脚本目录定位项目根目录，避免 Compose 使用错误工作目录。
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROJECT_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
COMPOSE_FILE="$PROJECT_ROOT/docker-compose.test.yml"
LIFECYCLE="$PROJECT_ROOT/scripts/docker-volume-lifecycle.py"

# 默认执行成功路径；--intentional-failure 用于验证失败时同样能够清理干净。
TEST_EXIT_CODE=0
if [ "${1:-}" = "--intentional-failure" ]; then
  TEST_EXIT_CODE=23
  shift
fi
if [ "$#" -ne 0 ]; then
  printf '用法：%s [--intentional-failure]\n' "$0" >&2
  exit 64
fi

# 每次运行使用唯一 project/run-id，避免并发任务共享容器或网络。
NOW=$(date +%s)
RUN_ID="ordinary-${NOW}-$$"
PROJECT_NAME="glr-${RUN_ID}"
EXPIRES_AT=$((NOW + 3600))

# 把同一份标签合同传入 Compose 服务和网络。
export GETLATESTREPO_TEST_PROJECT="$PROJECT_NAME"
export GETLATESTREPO_TEST_RUN_ID="$RUN_ID"
export GETLATESTREPO_TEST_CREATED_AT="$NOW"
export GETLATESTREPO_TEST_EXPIRES_AT="$EXPIRES_AT"
export GETLATESTREPO_TEST_EXIT_CODE="$TEST_EXIT_CODE"
export GETLATESTREPO_TEST_HOLD_SECONDS=0

# 下次启动先回收已过期且归属完整的普通测试资源，SIGKILL 残留不会永久堆积。
python3 "$LIFECYCLE" cleanup-expired --scope ordinary-test

# trap 只操作当前唯一 Compose project；即使收到 INT/TERM，也不会触碰其他项目。
cleanup_current_project() {
  docker compose \
    --project-name "$PROJECT_NAME" \
    --file "$COMPOSE_FILE" \
    down --volumes --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup_current_project EXIT INT TERM

# 先创建容器，再 inspect 真实挂载，防止镜像 VOLUME 暗中产生匿名卷。
docker compose \
  --project-name "$PROJECT_NAME" \
  --file "$COMPOSE_FILE" \
  create ordinary-test >/dev/null
CONTAINER_ID=$(docker compose \
  --project-name "$PROJECT_NAME" \
  --file "$COMPOSE_FILE" \
  ps --all --quiet ordinary-test)
python3 "$LIFECYCLE" assert-no-volume-mounts \
  --container "$CONTAINER_ID" \
  --require-tmpfs /data \
  --require-tmpfs /tmp

# 保留被测命令退出码；无论成功失败，后续都先清理并检查标签残留。
set +e
docker compose \
  --project-name "$PROJECT_NAME" \
  --file "$COMPOSE_FILE" \
  up --no-recreate --abort-on-container-exit --exit-code-from ordinary-test
COMMAND_STATUS=$?
set -e

# 显式清理后关闭 trap，避免正常退出时重复执行。
cleanup_current_project
trap - EXIT INT TERM

# 标签范围必须完全为空；这一检查与全局卷集合守卫互补。
python3 "$LIFECYCLE" assert-no-owned-resources \
  --scope ordinary-test \
  --run-id "$RUN_ID"

# 故障注入路径必须保留原始非零退出码，让 CI 能验证失败清理而不伪装成功。
exit "$COMMAND_STATUS"
