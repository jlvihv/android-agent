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

# ⚠️ 自己的 stdin 关掉：`adb shell` 会把 stdin 转发给远端命令，
# 于是调用方（`printf '/bash ls\n' | just phone` 这种管道）喂进来的东西
# 会被这里的第一条 adb 吃掉，后面的 python 客户端收到空 stdin 直接退出。
# 我们不需要 stdin，直接用 /dev/null。
exec 0</dev/null


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

# 开工前确认手机那头不会被冻住。
#
# App 一旦失去焦点，Android 的 cached app freezer 会在 60~75 秒后把它的**整个
# 进程**冻住（cgroup v2 freezer）—— Rust 核心、沙箱、WebView 全停，远程发过去的
# 请求就原地挂住，不报错也不超时；用户一碰手机瞬间全通。实测和两个机制见
# scripts/phone-wake.sh 顶部那张表。
#
# `auto` 只在**真需要时**才动手机（已经在前台就什么都不做），提示走 stderr ——
# 上面那行 URL 必须是最后一行（调用方普遍用 `tail -1` 取它）。
[ "${AGENT_NO_WAKE:-}" = "1" ] || scripts/phone-wake.sh auto >&2 || true

echo "http://127.0.0.1:$HOST_PORT"
