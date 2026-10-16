#!/usr/bin/env bash
# 真机调试的常用动作。source 进来用，或者直接跑。
#
#   . scripts/dev.sh
#   dev_pick          # 选中设备（结果放 $DEV）
#   dev_restart       # 重装 + 重启 App + 转发端口（结果放 $PORT）
#   dev_a11y_on       # 开启无障碍（保留已有的其它服务）
#   dev_llm           # 把 .env.local 里的 LLM 配置注入设备
#   dev_shell '<cmd>' # 在设备的 Linux 沙箱里跑命令
#
# 用法示例：
#   . scripts/dev.sh && dev_restart && dev_a11y_on && dev_llm && dev_shell 'uname -m'

set -u

# adb 用哪份、设备侧端口怎么找，都交给 scripts/ 里的共用脚本 ——
. "$(dirname "${BASH_SOURCE[0]}")/adb.sh"
APK="${APK:-android/app/build/outputs/apk/debug/app-debug.apk}"
PKG=dev.vihv.agent
A11Y="$PKG/$PKG.AgentAccessibilityService"
HOST_PORT="${HOST_PORT:-18787}"

# 无线调试会自己掉线（端口变了 / 手机休眠 / 切 Wi-Fi）。
# 掉了就用 mDNS 重新发现并 connect —— 别每次手动折腾。
dev_pick() {
  DEV="$(agent_dev || true)"
  if [ -z "$DEV" ]; then
    # 无线调试会自己掉线（端口变了 / 手机休眠 / 切 Wi-Fi），也可能电脑刚睡醒、
    # adb server 被重启过（显式连接不跨 server 重启）。拿上次记下的地址重连一次。
    echo "设备掉了，重连 $(phone_addr)…" >&2
    phone_reconnect || true
    sleep 2
    DEV="$(agent_dev || true)"
  fi
  if [ -z "$DEV" ]; then
    cat >&2 <<'EOF'
还连不上。手机上确认「开发者选项 → 无线调试」开着，然后带上那行的地址：
  just wifi <ip:port>
EOF
    return 1
  fi
  echo "设备: $DEV"
}

# 设备侧端口。四级兜底在 scripts/phone-port.sh 里；
# 刚 am start 完核心可能还没 bind，所以这里等一下。
dev_port() {
  local i p
  for i in $(seq 20); do
    p=$(ANDROID_SERIAL="$DEV" scripts/phone-port.sh 2>/dev/null || true)
    [ -n "$p" ] && { echo "$p"; return 0; }
    sleep 0.5
  done
  return 1
}

dev_restart() {
  dev_pick || return 1
  "$ADB" -s "$DEV" install -r "$APK" >/dev/null || return 1
  "$ADB" -s "$DEV" shell am force-stop "$PKG"
  "$ADB" -s "$DEV" logcat -c
  "$ADB" -s "$DEV" shell am start -n "$PKG/.MainActivity" >/dev/null
  sleep 9
  PORT="$(dev_port)"
  [ -n "$PORT" ] || { echo "拿不到端口：App 起来了吗？看 logcat -s AgentApp" >&2; return 1; }
  ANDROID_SERIAL="$DEV" HOST_PORT="$HOST_PORT" scripts/phone-forward.sh >/dev/null
  echo "http://127.0.0.1:$HOST_PORT  →  设备 :$PORT"
}

# force-stop 会解绑无障碍服务，重启后必须重新触发一次。
# 注意保留用户已有的其它服务（比如 Minis）。
dev_a11y_on() {
  [ -n "${DEV:-}" ] || dev_pick || return 1
  local cur others
  cur="$("$ADB" -s "$DEV" shell settings get secure enabled_accessibility_services | tr -d '\r')"
  others="$(echo "$cur" | tr ':' '\n' | grep -v "^$A11Y$" | grep -v '^null$' | paste -sd: -)"
  # 先摘掉再挂上，强制系统重新绑定
  "$ADB" -s "$DEV" shell settings put secure enabled_accessibility_services "$others"
  sleep 2
  "$ADB" -s "$DEV" shell settings put secure enabled_accessibility_services "${others:+$others:}$A11Y"
  "$ADB" -s "$DEV" shell settings put secure accessibility_enabled 1
  sleep 4
  # 核心的 /api 要凭据（docs/07 的「核心的接口要凭据」），顺手确认设备那条链路是通的
  tok="$(scripts/phone-token.sh 2>/dev/null || true)"
  curl -sS -m 10 -H "x-agent-token: $tok" "http://127.0.0.1:$HOST_PORT/api/health" \
    | python3 -c "import sys,json; print('device =', json.load(sys.stdin)['device'])" \
    || echo "device = ?（没拿到 /api/health，跑 just doctor 看）"
}

# 安卓上 App 不继承 shell 环境变量，所以运行时注入。
dev_llm() {
  set -a; . ./.env.local; set +a
  curl -sS -m 20 -X POST "http://127.0.0.1:$HOST_PORT/api/llm" \
    -H 'content-type: application/json' \
    -d "$(python3 -c "
import json,os
print(json.dumps({
  'name': os.environ.get('AGENT_LLM_NAME','opencode-go'),
  'baseUrl': os.environ['OPENCODE_BASE_URL'],
  'model': os.environ['OPENCODE_MODEL'],
  'apiKey': os.environ['OPENCODE_API_KEY'],
}))")"
  echo
}

# 直接在设备的 Linux 沙箱里跑命令（绕开 LLM）
dev_shell() {
  python3 -c "
import json,sys,urllib.request
req = urllib.request.Request(
    'http://127.0.0.1:$HOST_PORT/api/linux',
    data=json.dumps({'command': sys.argv[1], 'timeout_ms': 240000}).encode(),
    headers={'content-type': 'application/json'})
d = json.load(urllib.request.urlopen(req, timeout=300))
print(d['output'])
" "$1"
}
