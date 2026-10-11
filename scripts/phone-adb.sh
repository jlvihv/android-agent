#!/usr/bin/env bash
#
# 让 adb 走 WiFi。三种办法，按「省事」排：
#
#   scripts/phone-adb.sh            # 看看现在什么情况（连了谁 + 发现到的服务）
#   scripts/phone-adb.sh connect    # 自动发现并连上（配过对之后；mdns 找得到就行）
#   scripts/phone-adb.sh pair <ip:port> <6 位码>
#                                   # 首次配对（Android 11+）。地址和码都在手机上：
#                                   # 开发者选项 → 无线调试 → 使用配对码配对设备
#   scripts/phone-adb.sh tcpip      # 经典办法：USB 插一次就够，之后走 WiFi（5555）。
#                                   # 会自动从 `ip route` 取手机 WiFi 地址。
#   scripts/phone-adb.sh disconnect # 断开无线连接（USB 不受影响）
#
# 配过一次之后，平时的调试流程（just forward / just phone）就自动走 WiFi 了 ——
# 它们只依赖 adb，不关心底下是 USB 还是无线。
set -euo pipefail

. "$(dirname "$0")/adb.sh"

# mdns 发现：$1 = 服务类型
mdns_addr() {
  "$ADB" mdns services 2>/dev/null | awk -v s="$1" '$2 == s { print $3; exit }' || true
}

wifi_ip() {
  "$ADB" shell ip route 2>/dev/null | awk '/wlan0/ { print $9; exit }' | tr -d '\r' || true
}

need_mdns() {
  if ! "$ADB" mdns services >/dev/null 2>&1; then
    cat >&2 <<EOF
这份 adb 不支持 \`adb mdns\`（$("$ADB" version 2>/dev/null | tail -1)），没法自动发现。
两条路：
  - 换 SDK 里的 adb：AGENT_ADB=\$HOME/Android/Sdk/platform-tools/adb $0 $*
  - 手动给地址：$0 pair <ip:port> <6 位码>
EOF
    exit 1
  fi
}

case "${1:-status}" in
  status)
    "$ADB" devices -l
    echo
    if "$ADB" mdns services >/dev/null 2>&1; then
      "$ADB" mdns services
    else
      echo "(这份 adb 不支持 mdns 发现；无线调试仍然可以用 connect/pair 手动连)"
    fi
    ;;

  connect)
    if [ -n "${2:-}" ]; then
      addr="$2"
    else
      need_mdns connect
      addr=$(mdns_addr _adb-tls-connect._tcp)
      if [ -z "$addr" ]; then
        cat >&2 <<'EOF'
没发现无线调试服务。检查：
  - 手机和电脑在同一个 WiFi（不是访客网络 / 开了 AP 隔离）
  - 手机：开发者选项 → 无线调试 已打开，并且**停留在那个页面上**
  - 已经用 USB 连过？那用 `scripts/phone-adb.sh tcpip` 更省事
EOF
        exit 1
      fi
    fi
    "$ADB" connect "$addr"
    ;;

  pair)
    need_mdns pair
    addr="${2:-}"
    code="${3:-}"
    if [ -z "$addr" ] || [ -z "$code" ]; then
      cat >&2 <<'EOF'
用法：scripts/phone-adb.sh pair <ip:port> <6 位码>
两个都在手机上：开发者选项 → 无线调试 → 使用配对码配对设备
（那个对话框必须开着，关了就换新码了）
EOF
      exit 2
    fi
    "$ADB" pair "$addr" "$code"
    ;;

  tcpip)
    port="${2:-5555}"
    "$ADB" tcpip "$port"
    sleep 1
    ip=$(wifi_ip)
    if [ -z "$ip" ]; then
      echo "取不到手机的 WiFi 地址（wlan0）。手机连上 WiFi 了吗？" >&2
      exit 1
    fi
    "$ADB" connect "$ip:$port"
    ;;

  disconnect)
    "$ADB" disconnect
    ;;

  *)
    sed -n '3,20p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
