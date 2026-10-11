#!/usr/bin/env bash
#
# 挑一个能用的 adb。**source 它**，会设置好 $ADB：
#
#   . "$(dirname "$0")/adb.sh"
#   "$ADB" devices
#
# 为什么不用 PATH 里的那个：发行版自带的 adb 常常缺功能 —— 这台机器上
# `/usr/bin/adb`（37.0.0-android-tools）没有 `adb mdns`，而**无线调试的自动
# 发现全指望它**（`_adb-tls-connect._tcp` 那一行就是这么来的）。
# SDK 里那份（platform-tools 37.0.1）有。
#
# 注意：真正干活的是 adb **server**（5037 端口），客户端只是跟它说话。
# 混用新旧客户端时，谁的 server 先起来就决定了有没有 mdns —— 所以这里
# 统一优先用 SDK 那份，别让 PATH 里的旧版抢先把 server 起来。

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
