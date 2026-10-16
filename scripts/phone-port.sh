#!/usr/bin/env bash
#
# 手机上跑着的核心监听哪个端口。
#
# 端口启动时会先试固定值（AgentApp.PREFERRED_PORT），被占了才让系统随便分。
# 有几种办法拿到，按可靠性排：
#
#   1. 命令行参数 / $PHONE_PORT         —— 手动指定，永远优先
#   2. `run-as <pkg> cat files/port`    —— App 写的（**要 debug 包**）
#   3. 外部私有目录里的 port 文件        —— App 写的副本，`adb shell cat` 就能读，
#                                         release 包也能用（见 AgentApp.kt）
#   4. /proc/net/tcp 里按 uid 筛        —— 不依赖 App 配合，任何构建都能用
#   5. logcat 里那行 "WebView 应指向 …" —— 兜底：logcat 环形缓冲会被冲掉
#
# 4 和 5 **都要验一遍**：核心的 `/api/health` 不带凭据回 401、带凭据回 200；
# 别的服务器不会给这两个码。为什么要验：
#
#   - 4 的路子会捞出**同一个 uid 在 loopback 上的所有 LISTEN**，而沙箱里跑个
#     `python3 -m http.server 8765` 就是其中一个。直接 `head -1` 的话，`just phone` /
#     `doctor` / `forward` 全会被转发到那个服务器上（回来一句 `501 Unsupported
#     method`，看着像核心挂了）。所以逐个问 `/api/health`，只认那个应声的。
#   - 5 的路子可能是**上一次启动**留下的端口（App 重启过），验一下才知道还活着。
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

# 统一用 SDK 里那份 adb（见 scripts/adb.sh）
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
dev=$(agent_dev || true)
if [ -z "${dev:-}" ]; then
  cat >&2 <<'EOF'
没有连着的设备。先连上：
  just wifi <ip:port>   # 地址在手机上：开发者选项 → 无线调试
  just wifi-tcpip       # 或者：USB 插一次，之后自动走 WiFi
EOF
  exit 1
fi
export ANDROID_SERIAL="$dev"

# 问一句 `/api/health`：这个设备侧端口是不是核心。
# 不带凭据时核心回 401（要凭据），带凭据回 200 —— 别的服务器不会给这两个码。
PROBE_PORT="${PROBE_PORT:-18788}"
is_core() {
  "$ADB" -s "$dev" forward "tcp:$PROBE_PORT" "tcp:$1" >/dev/null 2>&1 || return 1
  local code
  code=$(curl -s -o /dev/null -m 3 -w '%{http_code}' "http://127.0.0.1:$PROBE_PORT/api/health" 2>/dev/null || true)
  "$ADB" -s "$dev" forward --remove "tcp:$PROBE_PORT" >/dev/null 2>&1 || true
  [ "$code" = "200" ] || [ "$code" = "401" ]
}

have_curl=0
command -v curl >/dev/null 2>&1 && have_curl=1

# --- 2) App 自己写的端口文件（内部目录，要 debug 包）------------------------
p=$("$ADB" shell "run-as $PKG cat files/port" 2>/dev/null | tr -d '\r\n' || true)
if [[ "${p:-}" =~ ^[0-9]+$ ]] && [ "$p" -gt 0 ]; then
  echo "$p"
  exit 0
fi

# --- 3) App 写的副本（外部私有目录）----------------------------------------
# `/sdcard/Android/data/<pkg>/files/port`：不经过 run-as，release 包也能读。
p=$("$ADB" shell "cat /sdcard/Android/data/$PKG/files/port" 2>/dev/null | tr -d '\r\n' || true)
if [[ "${p:-}" =~ ^[0-9]+$ ]] && [ "$p" -gt 0 ]; then
  echo "$p"
  exit 0
fi

# --- 4) /proc/net/tcp：该 uid 在 127.0.0.1 上的 LISTEN，逐个验 ----------------
uid=$("$ADB" shell "cmd package list packages -U $PKG" 2>/dev/null | tr -d '\r' \
  | sed -n 's/.*uid:\([0-9]*\).*/\1/p' | head -1 || true)
if [ -n "${uid:-}" ]; then
  hexes=$("$ADB" shell cat /proc/net/tcp 2>/dev/null | tr -d '\r' | awk -v u="$uid" '
    NR > 1 && $4 == "0A" && $8 == u && $2 ~ /^0100007F:/ { split($2, a, ":"); print a[2] }' || true)
  ports=""
  for h in $hexes; do ports="$ports $((16#$h))"; done
  ports="${ports# }"
  if [ -n "$ports" ]; then
    if [ "$have_curl" = "1" ]; then
      for cand in $ports; do
        if is_core "$cand"; then
          echo "$cand"
          exit 0
        fi
      done
      echo "候选端口（$ports）都没应答 /api/health —— 核心还活着吗？" >&2
    else
      # 没 curl 验不了：退回老行为，但说清楚
      echo "没有 curl，跳过 /api/health 验证（候选：$ports）—— 装个 curl 更稳" >&2
      echo "${ports%% *}"
      exit 0
    fi
  fi
fi

# --- 5) logcat 兜底，也验一遍 ----------------------------------------------
p=$("$ADB" logcat -d -s AgentApp:I 2>/dev/null | grep -o '127\.0\.0\.1:[0-9]*' | tail -1 | cut -d: -f2 || true)
if [ -n "${p:-}" ]; then
  # 这行可能是**上一次启动**留下的（logcat 是环形缓冲，App 重启过就旧了）
  if [ "$have_curl" = "0" ] || is_core "$p"; then
    echo "$p"
    exit 0
  fi
  echo "logcat 里那个端口 $p 没应答 —— 大概是上一次启动留下的" >&2
fi

cat >&2 <<'EOF'
找不到核心的端口。试一下：
  - 手机上的 App 开着吗？（核心跟着 Application 启动）
  - 无线调试连上了吗：just wifi
  - 重启一次 App（`adb shell am force-stop dev.vihv.agent` + 打开它）：会重新写 port 文件 / 打一行 logcat
  - 手动指定：scripts/phone-port.sh 37323，或 PHONE_PORT=37323
EOF
exit 1
