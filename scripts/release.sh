#!/bin/bash
# GetLatestRepo release build script

set -e

echo "=========================================="
echo "GetLatestRepo Release Build"
echo "=========================================="
echo ""

cd "$(dirname "$0")/.."

# Clean old build
echo "Cleaning old build..."
cargo clean

# test-all.sh 运行 release 二进制；clean 后必须先构建，否则测试脚本找不到目标文件。
echo ""
echo "Building release version for tests..."
cargo build --release

# Run tests
echo ""
echo "Running tests..."
./scripts/test-all.sh

# Rebuild release version after tests to ensure final artifact matches current source.
echo ""
echo "Building release version..."
cargo build --release

# Check build result
if [ ! -f "target/release/getlatestrepo" ]; then
    echo "✗ Build failed"
    exit 1
fi

echo ""
echo "✓ Build successful!"
echo ""
echo "Binary: target/release/getlatestrepo"
echo "File size: $(ls -lh target/release/getlatestrepo | awk '{print $5}')"
echo ""
echo "Running test:"
VERIFY_HOME="$(mktemp -d /tmp/getlatestrepo-release-home.XXXXXX)"
VERIFY_CONFIG="$(mktemp -d /tmp/getlatestrepo-release-config.XXXXXX)"
mkdir -p "$VERIFY_HOME/Library/Caches"
HOME="$VERIFY_HOME" GETLATESTREPO_CONFIG_DIR="$VERIFY_CONFIG" ./target/release/getlatestrepo --version
rm -rf "$VERIFY_HOME" "$VERIFY_CONFIG"

echo ""
echo "Release checklist:"
echo "  [ ] Version number updated"
echo "  [ ] CHANGELOG updated"
echo "  [ ] All tests passed"
echo "  [ ] Documentation updated"
