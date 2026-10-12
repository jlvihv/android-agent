#!/usr/bin/env bash
#
# 安卓模拟器（x86_64，KVM 加速）—— 给"改完就看"用，不动用户的手机。
#
# 为什么需要它：真机是无线调试（mDNS + TLS），手机一睡、用户一用、VPN 一抖，
# 链路就掉；而且**不能在用户正在用手机的时候拿 `input tap` 去戳界面**。
# 跑 UI 验收（面板、手势、截图）必须在自己的设备上。
#
# 已知限制：**沙箱在这上面跑不了**。proot 和 Alpine rootfs 都是 aarch64 的
# （`android-assets` 从 OpenMinis 拷的就是 arm64），x86_64 上 exec 不了。
# 所以模拟器能验：UI / 浏览器 / 搜索 / LLM / 会话库；不能验：bash / edit / read。
#
# 用法：
#   scripts/emu.sh start     起（无窗口，后台；已起就什么都不做）
#   scripts/emu.sh wait      等开机完成
#   scripts/emu.sh serial    打印 serial（配 ANDROID_SERIAL 用）
#   scripts/emu.sh stop      关
#   scripts/emu.sh status    在跑吗
set -euo pipefail
cd "$(dirname "$0")/.."

. scripts/adb.sh

AVD="${AGENT_AVD:-agent-x86}"
SDK="${ANDROID_HOME:-$HOME/Android/Sdk}"
EMU="$SDK/emulator/emulator"
PORT="${AGENT_EMU_PORT:-5554}"
SERIAL="emulator-$PORT"

say() { printf '\033[1m%s\033[0m\n' "$*"; }

[ -x "$EMU" ] || {
  echo "没有 emulator：$EMU" >&2
  echo "装：sdkmanager --install emulator 'system-images;android-35;google_apis;x86_64'" >&2
  exit 1
}

case "${1:-status}" in
  start)
    if "$ADB" devices | awk '{print $1}' | grep -qx "$SERIAL"; then
      say "$SERIAL 已经在跑"
      exit 0
    fi
    say "启动 $AVD（无窗口 + swiftshader 渲染）"
    # -no-window：不要窗口，截图走 `adb exec-out screencap`，App 自己的截图走
    #   软件绘制 —— 两样都不需要窗口。
    # -gpu swiftshader_indirect：host 上没有能用的 GL，软渲染最稳。
    # -no-snapshot-save：每次干净启动（省得把上次的脏状态带进验收）。
    nohup "$EMU" -avd "$AVD" -port "$PORT" \
      -no-window -no-audio -no-boot-anim -no-snapshot-save \
      -gpu swiftshader_indirect -netdelay none -netspeed full \
      >/tmp/agent-emu.log 2>&1 &
    say "后台起来了（日志 /tmp/agent-emu.log），等开机用：scripts/emu.sh wait"
    ;;
  wait)
    say "等 $SERIAL 开机…"
    "$ADB" -s "$SERIAL" wait-for-device
    # sys.boot_completed 之前 adb 就有了，但包管理还没起来
    for _ in $(seq 1 120); do
      if [ "$("$ADB" -s "$SERIAL" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = "1" ]; then
        say "开机完成"
        exit 0
      fi
      sleep 2
    done
    echo "等开机超时（看 /tmp/agent-emu.log）" >&2
    exit 1
    ;;
  serial) echo "$SERIAL" ;;
  stop)
    "$ADB" -s "$SERIAL" emu kill 2>/dev/null || true
    say "已发 kill"
    ;;
  status)
    if "$ADB" devices | awk '{print $1}' | grep -qx "$SERIAL"; then
      echo "$SERIAL 在跑"
    else
      echo "$SERIAL 没跑"
    fi
    ;;
  *)
    sed -n 's/^# \{0,1\}//p' "$0" | sed -n '2,20p'
    exit 2
    ;;
esac
