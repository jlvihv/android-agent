#!/usr/bin/env bash
#
# 手机上跑着的核心监听哪个端口。
#
# 端口是启动时随机分的（Kotlin 侧 `core.start(0)`），所以只能从 logcat 里捞。
# 优先级：命令行参数 > $PHONE_PORT > logcat。
#
#   scripts/phone-port.sh 41234
#
set -euo pipefail

if [ "${1:-}" != "" ]; then
  echo "$1"
  exit 0
fi
if [ "${PHONE_PORT:-}" != "" ]; then
  echo "$PHONE_PORT"
  exit 0
fi

port=$(
  adb logcat -d -s AgentApp:I 2>/dev/null \
    | grep -o '127\.0\.0\.1:[0-9]*' \
    | tail -1 \
    | cut -d: -f2 || true
)

if [ -z "${port:-}" ]; then
  cat >&2 <<'EOF'
找不到核心的端口。试一下：
  - 把手机上的 App 打开一次（启动时会打一行 "WebView 应指向 http://127.0.0.1:<port>"）
  - 或者手动指定：scripts/phone-port.sh 41234
  - 或者设环境变量：PHONE_PORT=41234
EOF
  exit 1
fi

echo "$port"
