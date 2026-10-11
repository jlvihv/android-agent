#!/usr/bin/env bash
#
# 手机上跑着的核心监听哪个端口。
#
# 端口是启动时随机分的（Kotlin 侧 `core.start(0)`），有四种办法拿到，按可靠性排：
#
#   1. 命令行参数 / $PHONE_PORT         —— 手动指定，永远优先
#   2. `run-as <pkg> cat files/port`    —— App 启动时自己写的，最准（要 debug 包）
#   3. /proc/net/tcp 里按 uid 筛        —— 不依赖 App 配合，任何构建都能用
#   4. logcat 里那行 "WebView 应指向 …" —— 兜底：logcat 环形缓冲会被冲掉（实测踩过）
#
# 3 是关键：新装的 App 还没写 files/port，而 logcat 里那行可能早就没了 ——
# 那时候只剩「按 uid 找 loopback 监听端口」这条路。
#
#   scripts/phone-port.sh          # 自动
#   scripts/phone-port.sh 37323    # 手动
set -euo pipefail

# ⚠️ 自己的 stdin 关掉：`adb shell` 会把 stdin 转发给远端命令，
# 于是调用方（`printf '/bash ls\n' | just phone` 这种管道）喂进来的东西
# 会被这里的第一条 adb 吃掉，后面的 python 客户端收到空 stdin 直接退出。
# 我们不需要 stdin，直接用 /dev/null。
exec 0</dev/null


PKG=dev.vihv.agent

# 统一用 SDK 里那份 adb（发行版自带的可能没有 mdns 等功能）
. "$(dirname "$0")/adb.sh"

if [ "${1:-}" != "" ]; then
  echo "$1"
  exit 0
fi
if [ "${PHONE_PORT:-}" != "" ]; then
  echo "$PHONE_PORT"
  exit 0
fi

"$ADB" wait-for-device >/dev/null 2>&1 || true

# --- 2) App 自己写的端口文件 -------------------------------------------------
p=$("$ADB" shell "run-as $PKG cat files/port" 2>/dev/null | tr -d '\r\n' || true)
if [[ "${p:-}" =~ ^[0-9]+$ ]] && [ "$p" -gt 0 ]; then
  echo "$p"
  exit 0
fi

# --- 3) /proc/net/tcp：该 uid 在 127.0.0.1 上的 LISTEN ----------------------
uid=$("$ADB" shell "cmd package list packages -U $PKG" 2>/dev/null | tr -d '\r' \
  | sed -n 's/.*uid:\([0-9]*\).*/\1/p' | head -1 || true)
if [ -n "${uid:-}" ]; then
  hex=$("$ADB" shell cat /proc/net/tcp 2>/dev/null | tr -d '\r' | awk -v u="$uid" '
    NR > 1 && $4 == "0A" && $8 == u && $2 ~ /^0100007F:/ { split($2, a, ":"); print a[2] }' \
    | head -1 || true)
  if [ -n "${hex:-}" ]; then
    echo "$((16#$hex))"
    exit 0
  fi
fi

# --- 4) logcat 兜底 ---------------------------------------------------------
p=$("$ADB" logcat -d -s AgentApp:I 2>/dev/null | grep -o '127\.0\.0\.1:[0-9]*' | tail -1 | cut -d: -f2 || true)
if [ -n "${p:-}" ]; then
  echo "$p"
  exit 0
fi

cat >&2 <<'EOF'
找不到核心的端口。试一下：
  - 手机上的 App 开着吗？（核心跟着 Application 启动）
  - 无线调试连上了吗：just wifi
  - 手动指定：scripts/phone-port.sh 37323，或 PHONE_PORT=37323
EOF
exit 1
