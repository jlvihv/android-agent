#!/usr/bin/env bash
#
# 让 adb 走 WiFi。地址在手机上：开发者选项 → 无线调试（**每台手机每开一次都
# 可能换端口**，所以地址要敲一次给它，之后脚本自己记着）。
#
#   scripts/phone-adb.sh            # 看看现在什么情况（连了谁 + 记着的地址）
#   scripts/phone-adb.sh connect <ip:port>
#                                   # 连上并记住；不带地址就用上次记住的那个
#   scripts/phone-adb.sh pair <ip:port> <6 位码>
#                                   # 首次配对（Android 11+）
#   scripts/phone-adb.sh tcpip      # 经典办法：USB 插一次就够，之后走 WiFi（5555）。
#                                   # 会自动从 `ip route` 取手机 WiFi 地址。
#   scripts/phone-adb.sh disconnect # 断开无线连接（USB 不受影响）
#
# 配过一次之后，平时的调试流程（just forward / just phone）就自动走 WiFi 了 ——
# 它们只依赖 adb，不关心底下是 USB 还是无线。
set -euo pipefail

# ⚠️ 自己的 stdin 关掉：`adb shell` 会把 stdin 转发给远端命令，
# 于是调用方（`printf '/bash ls\n' | just phone` 这种管道）喂进来的东西
# 会被这里的第一条 adb 吃掉，后面的 python 客户端收到空 stdin 直接退出。
# 我们不需要 stdin，直接用 /dev/null。
exec 0</dev/null


. "$(dirname "$0")/adb.sh"

wifi_ip() {
  "$ADB" shell ip route 2>/dev/null | awk '/wlan0/ { print $9; exit }' | tr -d '\r' || true
}

usage_connect() {
  cat >&2 <<EOF
用法：$0 connect <ip:port>
地址在手机上：开发者选项 → 无线调试（页面上那行「IP 地址和端口」）。
连过一次之后就记着了，以后不带地址也行。
EOF
  exit 2
}

case "${1:-status}" in
  status)
    "$ADB" devices -l
    a="$(phone_addr)"
    [ -n "$a" ] && echo "记着的地址: $a"
    ;;

  connect)
    addr="${2:-$(phone_addr)}"
    [ -n "$addr" ] || usage_connect
    "$ADB" connect "$addr" && phone_remember "$addr"
    ;;

  pair)
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
    "$ADB" connect "$ip:$port" && phone_remember "$ip:$port"
    ;;

  disconnect)
    "$ADB" disconnect
    ;;

  *)
    sed -n '3,20p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
