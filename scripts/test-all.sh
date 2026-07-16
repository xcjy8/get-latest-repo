#!/bin/bash
# GetLatestRepo full-command test script

set -e  # Exit immediately on error

echo "=========================================="
echo "GetLatestRepo Full-Command Automated Test"
echo "=========================================="
echo ""

GETLATESTREPO="./target/release/getlatestrepo"

# Color definitions
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m' # No Color

# Test counters
PASSED=0
FAILED=0

# Test function
run_test() {
    local name="$1"
    local cmd="$2"
    echo -n "Test: $name ... "
    if eval "$cmd" > /tmp/test_output.txt 2>&1; then
        echo -e "${GREEN}✓ PASS${NC}"
        PASSED=$((PASSED + 1))
    else
        echo -e "${RED}✗ FAIL${NC}"
        echo "  Error output:"
        cat /tmp/test_output.txt | sed 's/^/    /'
        FAILED=$((FAILED + 1))
    fi
}

# 断言命令必须失败，避免 `失败 && false || true` 把意外成功也伪装成通过。
run_test_failure() {
    local name="$1"
    local cmd="$2"
    echo -n "Test: $name ... "
    if eval "$cmd" > /tmp/test_output.txt 2>&1; then
        echo -e "${RED}✗ FAIL${NC}"
        echo "  命令意外成功，预期应返回非零退出码"
        FAILED=$((FAILED + 1))
    else
        echo -e "${GREEN}✓ PASS${NC}"
        PASSED=$((PASSED + 1))
    fi
}

# Ensure we are in the correct directory
cd "$(dirname "$0")/.."

# 使用临时配置目录隔离测试环境
export GETLATESTREPO_CONFIG_DIR="/tmp/rg-test-config-$$"
export HOME="/tmp/rg-test-home-$$"
export XDG_CACHE_HOME="/tmp/rg-test-cache-$$"
mkdir -p "$GETLATESTREPO_CONFIG_DIR"
mkdir -p "$HOME/Library/Caches" "$XDG_CACHE_HOME"

# Clean previous test data
echo "Cleaning test environment..."
rm -rf /tmp/rg-test-dir /tmp/rg-init-test /tmp/test-repos "$HOME" "$XDG_CACHE_HOME" 2>/dev/null || true
mkdir -p "$GETLATESTREPO_CONFIG_DIR" "$HOME/Library/Caches" "$XDG_CACHE_HOME"

# 创建测试目录并在其中初始化 Git 仓库（供 workflow check 扫描）
mkdir -p /tmp/rg-test-dir
if [ ! -d /tmp/rg-test-dir/.git ]; then
    git init /tmp/rg-test-dir >/dev/null 2>&1
    cd /tmp/rg-test-dir
    git config user.email "test@test.com"
    git config user.name "Test"
    touch README.md
    git add README.md >/dev/null 2>&1
    git commit -m "init" >/dev/null 2>&1
    cd - >/dev/null
fi

echo "1. Basic command tests"
echo "----------------------"
run_test "help" "$GETLATESTREPO --help"
run_test "version" "$GETLATESTREPO --version"

echo ""
echo "2. config command tests"
echo "-----------------------"
run_test "config list" "$GETLATESTREPO config list"
run_test "config path" "$GETLATESTREPO config path"
run_test "config add" "$GETLATESTREPO config add /tmp/rg-test-dir"
run_test_failure "config add (duplicate)" "$GETLATESTREPO config add /tmp/rg-test-dir"
run_test "config ignore" "$GETLATESTREPO config ignore '*.log,*.tmp'"

echo ""
echo "3. init command tests"
echo "---------------------"
mkdir -p /tmp/rg-init-test
run_test "init" "$GETLATESTREPO init /tmp/rg-init-test"

echo ""
echo "4. status command tests"
echo "-----------------------"
# 创建测试用 Git 仓库
mkdir -p /tmp/test-repos/project-a
if [ ! -d /tmp/test-repos/project-a/.git ]; then
    git init /tmp/test-repos/project-a >/dev/null 2>&1
fi
run_test "status (valid repo)" "$GETLATESTREPO status /tmp/test-repos/project-a"
run_test_failure "status (invalid path)" "$GETLATESTREPO status /nonexistent"
run_test "status --issues" "$GETLATESTREPO status /tmp/test-repos/project-a --issues"

echo ""
echo "5. workflow command tests (dry-run)"
echo "------------------------------------"
run_test "workflow --list" "$GETLATESTREPO workflow --list"
run_test "workflow check --dry-run" "$GETLATESTREPO workflow check --dry-run"
run_test "workflow daily --dry-run" "$GETLATESTREPO workflow daily --dry-run"
run_test "workflow report --dry-run" "$GETLATESTREPO workflow report --dry-run"
run_test "workflow ci --dry-run" "$GETLATESTREPO workflow ci --dry-run"
run_test "workflow pull-safe --dry-run" "$GETLATESTREPO workflow pull-safe --dry-run"
run_test "workflow pull-force --dry-run" "$GETLATESTREPO workflow pull-force --dry-run"
run_test "workflow pull-backup --dry-run" "$GETLATESTREPO workflow pull-backup --dry-run"

echo ""
echo "6. workflow command tests (actual execution)"
echo "--------------------------------------------"
run_test "workflow check" "$GETLATESTREPO workflow check"

echo ""
echo "7. cleanup tests"
echo "----------------"
run_test "config remove" "$GETLATESTREPO config remove /tmp/rg-test-dir"

# Cleanup
rm -rf /tmp/rg-test-dir /tmp/rg-init-test /tmp/test-repos "$GETLATESTREPO_CONFIG_DIR" "$HOME" "$XDG_CACHE_HOME"

echo ""
echo "=========================================="
echo "Tests completed"
echo "=========================================="
echo -e "Passed: ${GREEN}$PASSED${NC}"
echo -e "Failed: ${RED}$FAILED${NC}"
echo ""

if [ $FAILED -eq 0 ]; then
    echo -e "${GREEN}All tests passed!${NC}"
    exit 0
else
    echo -e "${RED}Some tests failed, please check!${NC}"
    exit 1
fi
