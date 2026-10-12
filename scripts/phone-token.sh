#!/usr/bin/env bash
#
# 手机上核心的访问凭据（`/api/*` 要它，见 core/src/state.rs 的 `Auth`）。
#
# 为什么要凭据：手机上任何一个有网络权限的 App 都能连 127.0.0.1，扫一遍端口
# 就能找到核心、在沙箱里跑命令。凭据跨重启不变（存在 sessions.db 里），拿一次就够。
#
# 三种办法，按可靠性排：
#
#   1. $AGENT_TOKEN                       —— 手动指定，永远优先
#   2. `run-as <pkg> cat files/token`     —— App 启动时写的（要 debug 包）
#   3. logcat 里那行 "agent token: …"     —— release 包只能靠它（logcat 只有
#                                            adb / 系统读得到别的 App 的日志）
#
#   scripts/phone-token.sh
set -euo pipefail
exec 0</dev/null

PKG=dev.vihv.agent
. "$(dirname "$0")/adb.sh"

if [ "${AGENT_TOKEN:-}" != "" ]; then
  echo "$AGENT_TOKEN"
  exit 0
fi

t=$("$ADB" shell "run-as $PKG cat files/token" 2>/dev/null | tr -d '\r\n' || true)
if [[ "${t:-}" =~ ^[0-9a-f]{32}$ ]]; then
  echo "$t"
  exit 0
fi

t=$("$ADB" logcat -d -s AgentApp 2>/dev/null | tr -d '\r' \
  | sed -n 's/.*agent token: \([0-9a-f]\{32\}\).*/\1/p' | tail -1 || true)
if [ -n "${t:-}" ]; then
  echo "$t"
  exit 0
fi

echo "拿不到凭据：debug 包用 run-as 读 files/token；release 包重启一次 App 再试（看 logcat）。" >&2
echo "或者手动：AGENT_TOKEN=… $0" >&2
exit 1
