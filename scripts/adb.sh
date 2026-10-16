#!/usr/bin/env bash
#
# 挑一个能用的 adb。**source 它**，会设置好 $ADB：
#
#   . "$(dirname "$0")/adb.sh"
#   "$ADB" devices
#
# 为什么不用 PATH 里的那个：发行版自带的 adb 常常是旧版（缺功能、和 Android
# Studio 抢 server）。真正干活的是 adb **server**（5037 端口），客户端只是跟它
# 说话 —— 所以这里统一优先用 SDK 那份，别让 PATH 里的旧版抢先把 server 起来。

if [ -n "${AGENT_ADB:-}" ]; then
  ADB="$AGENT_ADB"
else
  ADB=""
  for c in \
    "${ANDROID_HOME:-}/platform-tools/adb" \
    "$HOME/Android/Sdk/platform-tools/adb" \
    "$HOME/Library/Android/sdk/platform-tools/adb"; do
    if [ -n "$c" ] && [ -x "$c" ]; then
      ADB="$c"
      break
    fi
  done
  [ -n "$ADB" ] || ADB="adb"
fi

export ADB

# **关掉 mDNS 发现**。它会在手机那条显式连接之外再自动连一条 transport，
# 于是同一台手机在 `adb devices` 里出现两次：不设 ANDROID_SERIAL 的命令直接
# 报 "more than one device"，拿到的是哪台也说不准。无线调试只走一条路：
# `adb connect <ip:port>`（`just wifi <ip:port>`）。
#
# 注意：这个变量只有 adb **server 启动那一刻**读得到（server 是那个 5037
# 端口上的常驻进程，客户端只是跟它说话）—— 所以下面 agent_dev 看到 mDNS
# 那条 transport 就重起一次 server，把它拉回正轨。
export ADB_MDNS=0

# 无线调试记下来的地址（`just wifi` 写，掉线时按它重连）。
AGENT_PHONE_ADDR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.dev/phone-addr"

# 选一台设备（结果同时设进 ANDROID_SERIAL，返回设备名；没有设备返回 1）。
#
# 先确认 server 没在跑 mDNS 自动连接：Android Studio 或之前的 shell 启的那份
# 是默认开着的，而上面那个变量对它没用（server 启动时就读过了）。看到那种
# transport 就重起 server（带上 ADB_MDNS=0），再按记下的地址把手机接回来。
agent_dev() {
  local d
  if "$ADB" devices 2>/dev/null \
      | awk '$2 == "device" && index($1, "_adb-tls-connect") { f = 1 } END { exit !f }'; then
    "$ADB" kill-server >/dev/null 2>&1 || true
    "$ADB" start-server >/dev/null 2>&1 || true
    phone_reconnect || true
    sleep 1
  fi
  d=$("$ADB" devices 2>/dev/null | awk '$2 == "device" { print $1; exit }')
  [ -n "$d" ] || return 1
  export ANDROID_SERIAL="$d"
  printf '%s\n' "$d"
}

# 记下的无线调试地址（没有就什么都不输出）。
phone_addr() {
  [ -s "$AGENT_PHONE_ADDR" ] && cat "$AGENT_PHONE_ADDR"
  return 0
}

phone_remember() {
  mkdir -p "$(dirname "$AGENT_PHONE_ADDR")"
  printf '%s\n' "$1" > "$AGENT_PHONE_ADDR"
}

# 掉线时按记下的地址重连一次（无线调试的端口会变、电脑睡过、切了 WiFi）。
phone_reconnect() {
  local a
  a="$(phone_addr)"
  [ -n "$a" ] || return 1
  "$ADB" connect "$a" >/dev/null 2>&1
}
