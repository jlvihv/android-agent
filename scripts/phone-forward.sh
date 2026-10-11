#!/usr/bin/env bash
#
# 把手机上跑着的核心转发到本机的一个**固定**端口，并打印它的地址。
#
#   scripts/phone-forward.sh          # → http://127.0.0.1:18787
#   HOST_PORT=9000 scripts/phone-forward.sh
#
# 固定本机端口是有意的：设备侧端口每次重启都变（`core.start(0)` 让系统分配），
# 而脚本、书签、肌肉记忆里要的是一个稳定地址。
#
# 设备侧的端口怎么找，见 scripts/phone-port.sh（四级兜底）。
set -euo pipefail

. "$(dirname "$0")/adb.sh"

HOST_PORT="${HOST_PORT:-18787}"

dev=$("$ADB" devices | awk '$2 == "device" { print $1; exit }')
if [ -z "${dev:-}" ]; then
  cat >&2 <<'EOF'
没有连着的设备。先连上：
  just wifi          # 已经配对过（自动发现）
  just wifi-tcpip    # 或者：USB 插一次，之后自动走 WiFi
  just wifi-pair <ip:port> <6 位码>    # 首次
EOF
  exit 1
fi
export ANDROID_SERIAL="$dev"

port=$(scripts/phone-port.sh)
"$ADB" -s "$dev" forward "tcp:$HOST_PORT" "tcp:$port" >/dev/null

echo "http://127.0.0.1:$HOST_PORT"
