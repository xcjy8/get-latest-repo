#!/bin/sh

set -eu

repository_root="${GETLATESTREPO_CONTAINER_SCAN_ROOT:-/repositories}"
backend_port="${GETLATESTREPO_BACKEND_PORT:-8615}"
config_file="${GETLATESTREPO_CONFIG_DIR:-/data}/config.toml"
proxy_url="${GETLATESTREPO_PROXY_URL:-http://host.docker.internal:7890}"

if [ ! -d "$repository_root" ]; then
    printf '✗ Docker 仓库挂载不存在：%s\n' "$repository_root" >&2
    exit 1
fi

if [ ! -s "$config_file" ]; then
    printf 'ℹ 首次启动：初始化容器扫描源并建立仓库索引。\n'
    getlatestrepo init "$repository_root"
    getlatestrepo scan --output terminal
fi

exec getlatestrepo \
    --proxy-url "$proxy_url" \
    serve \
    --bind 0.0.0.0 \
    --port "$backend_port" \
    --no-open
