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

ADB="${ADB:-$HOME/Android/Sdk/platform-tools/adb}"
APK="${APK:-android/app/build/outputs/apk/debug/app-debug.apk}"
PKG=dev.vihv.agent
A11Y="$PKG/$PKG.AgentAccessibilityService"
HOST_PORT="${HOST_PORT:-18787}"

# 无线调试会自己掉线（端口变了 / 手机休眠 / 切 Wi-Fi）。
# 掉了就用 mDNS 重新发现并 connect —— 别每次手动折腾。
dev_pick() {
  DEV="$("$ADB" devices | awk '$2=="device"{print $1; exit}')"
  if [ -z "$DEV" ]; then
    echo "设备掉了，用 mDNS 重连…" >&2
    "$ADB" kill-server >/dev/null 2>&1
    sleep 1
    "$ADB" start-server >/dev/null 2>&1
    sleep 4
    local addr
    addr="$("$ADB" mdns services 2>/dev/null | awk '/_adb-tls-connect/{print $3; exit}')"
    if [ -n "$addr" ]; then
      timeout 15 "$ADB" connect "$addr" >/dev/null 2>&1
      sleep 2
      DEV="$("$ADB" devices | awk '$2=="device"{print $1; exit}')"
    fi
  fi
  if [ -z "$DEV" ]; then
    echo "还是连不上。检查手机的无线调试是否开着（它会自己关）。" >&2
    "$ADB" mdns services >&2
    return 1
  fi
  echo "设备: $DEV"
}

# 从 logcat 读 App 实际监听的端口
dev_port() {
  "$ADB" -s "$DEV" logcat -d -s AgentApp 2>/dev/null \
    | grep -o '127\.0\.0\.1:[0-9]*' | tail -1 | cut -d: -f2
}

dev_restart() {
  dev_pick || return 1
  "$ADB" -s "$DEV" install -r "$APK" >/dev/null || return 1
  "$ADB" -s "$DEV" shell am force-stop "$PKG"
  "$ADB" -s "$DEV" logcat -c
  "$ADB" -s "$DEV" shell am start -n "$PKG/.MainActivity" >/dev/null
  sleep 9
  PORT="$(dev_port)"
  [ -n "$PORT" ] || { echo "拿不到端口，看 logcat -s AgentApp" >&2; return 1; }
  "$ADB" -s "$DEV" forward --remove-all >/dev/null 2>&1
  "$ADB" -s "$DEV" forward "tcp:$HOST_PORT" "tcp:$PORT" >/dev/null
  echo "http://127.0.0.1:$HOST_PORT  →  设备 :$PORT"
}

# force-stop 会解绑无障碍服务，重启后必须重新触发一次。
# 注意保留用户已有的其它服务（比如 Minis）。
dev_a11y_on() {
  local cur others
  cur="$("$ADB" -s "$DEV" shell settings get secure enabled_accessibility_services | tr -d '\r')"
  others="$(echo "$cur" | tr ':' '\n' | grep -v "^$A11Y$" | grep -v '^null$' | paste -sd: -)"
  # 先摘掉再挂上，强制系统重新绑定
  "$ADB" -s "$DEV" shell settings put secure enabled_accessibility_services "$others"
  sleep 2
  "$ADB" -s "$DEV" shell settings put secure enabled_accessibility_services "${others:+$others:}$A11Y"
  "$ADB" -s "$DEV" shell settings put secure accessibility_enabled 1
  sleep 4
  curl -sS -m 10 "http://127.0.0.1:$HOST_PORT/api/health" \
    | python3 -c "import sys,json; print('device =', json.load(sys.stdin)['device'])"
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
