#!/usr/bin/env bash
#
# 让手机上那个 App **别被系统冻住** —— 远程驱动（`just phone` / agentctl）开工前跑一次。
#
# 症状：手机闲置一会儿（60~75 秒）之后，从电脑发过去的请求**原地挂住** ——
# 不报错、不超时，`curl` 就一直等；用户一碰手机（App 到前台），瞬间全通了。
#
# 真因不是 WebView，是 **Android 的 cached app freezer**（cgroup v2 freezer）。
# 冻住的时候**整个进程都不跑**：Rust 核心收不到请求（连接在 kernel 里排队、
# 没人去 accept）、沙箱跑不了、WebView 也不动 —— 连 `echo` 都跑不了
# （`/api/health` 这种纯本地、不碰 WebView 的请求一样挂）。
#
# （Pixel 10 / Android 17，`/sys/fs/cgroup/apps/uid_X/pid_Y/cgroup.freeze`）：
#
#   | 状态                                  | freeze | /api/health |
#   |---------------------------------------|--------|-------------|
#   | App 是焦点 Activity（屏幕亮 / 熄屏都好）| 0      | 25ms        |
#   | 失去焦点后 60~75 秒（按 HOME）          | **1**  | 挂死        |
#   | 失去焦点 + 加到 doze 白名单             | **1**  | 挂死        |
#   | 熄屏很久 → 强制深度 doze（无白名单）     | **1**  | 挂死        |
#   | 熄屏很久 → 强制深度 doze（有白名单）     | 0      | 24ms        |
#   | 冻结后再叫到前台                        | 0      | 24ms        |
#
# 所以是**两个机制**，要两招分别治：
#
#   1. **失去焦点（前一个 App）→ 60~75 秒后被冻**：白名单治不了。
#      治法是让它保持焦点：`am start` 把我们的 Activity 提到前面（锁屏挡着也算，
#      熄屏 Dozing 下也稳）。
#   2. **深度 doze（熄屏 30 分钟以上）→ 也被冻，而且联网会被掐**：
#      这个白名单（设置里「电池 → 不受限」）能治。
#
# 用法：
#   scripts/phone-wake.sh          # 看现在什么情况（默认）
#   scripts/phone-wake.sh on       # 白名单 + 叫到前台（开工前跑这个）
#   scripts/phone-wake.sh off      # 把 doze 白名单摘掉
set -euo pipefail
exec 0</dev/null

. "$(dirname "$0")/adb.sh"

PKG=dev.vihv.agent
ICON=dev.vihv.agent/.MainActivity

# ⚠️ 所有 dumpsys 都先整份读进来再 grep。
# 直接 `dumpsys … | grep -m1` 会让 adb 吃到 broken pipe 而**非零退出**，
# 配上 set -o pipefail 就变成一个永远失败的判断（明明在前台也会报「不是」）。
dump() { "$ADB" shell "dumpsys $1" 2>/dev/null | tr -d '\r' || true; }

# oom adj 才是 freezer 真正看的东西：0 = 前台（不会被冻），700 / 900+ = cached（会被冻）。
adj() {
  dump "activity processes" | grep -A22 "ProcessRecord.*$PKG" \
    | sed -n 's/.*oom adj:.*cur=\([0-9-]*\).*/\1/p' | head -1
}

focused() {
  dump "activity activities" | grep -m1 -E 'mFocusedApp=' | grep -q "$PKG"
}

frozen_flag() {
  local pid cg f
  pid=$("$ADB" shell pidof "$PKG" 2>/dev/null | tr -d '\r' || true)
  [ -n "$pid" ] || { echo "没在跑"; return; }
  # 0::/apps/uid_10306/pid_9724 —— Android 14+ 的 cgroup v2 统一层级
  cg=$("$ADB" shell "cat /proc/$pid/cgroup 2>/dev/null" | tr -d '\r' \
       | sed -n 's|^0::/apps/\(.*\)$|\1|p' | head -1)
  [ -n "$cg" ] || { echo "查不到（老内核 / 没有 freezer）"; return; }
  f=$("$ADB" shell "cat /sys/fs/cgroup/apps/$cg/cgroup.freeze 2>/dev/null" | tr -d '\r' || true)
  case "${f:-}" in
    1) echo "**冻结中**" ;;
    0) echo "正常" ;;
    *) echo "查不到" ;;
  esac
}

whitelisted() {
  dump "deviceidle whitelist" | grep -q "$PKG"
}

report() {
  local a
  a=$(adj)
  if focused; then
    printf '  焦点 Activity：我们的（oom adj=%s → 不会被冻）\n' "${a:-?}"
  else
    printf '  焦点 Activity：不是我们的（oom adj=%s → 60~75 秒后会被冻）\n' "${a:-?}"
  fi
  printf '  doze 白名单：  %s\n' \
    "$(whitelisted && echo "在（深度 doze 里不会被冻、联网也不掐）" || echo "不在（深度 doze 里会被冻、联网被掐）")"
  printf '  进程：         %s\n' "$(frozen_flag)"
}

case "${1:-status}" in
  status)
    report
    ;;

  # 给 phone-forward.sh 用的静默版：**只在真需要动手机时才动**。
  # 已经在前台就一个字都不说（不抢焦点、不产生副作用）。
  auto)
    if focused; then
      exit 0
    fi
    if [ -z "$("$ADB" shell pidof "$PKG" 2>/dev/null | tr -d '\r' || true)" ]; then
      echo "⚠️  手机上 $PKG 没在跑（先启动一次 App）" >&2
      exit 0
    fi
    "$ADB" shell "am start -n $ICON" >/dev/null 2>&1 || true
    echo "⚠️  手机上那个 App 不在前台（系统 60~75 秒后会把整个进程冻住，远程回合会挂住）" >&2
    echo "    已把它叫到前台；不想被抢焦点就 AGENT_NO_WAKE=1" >&2
    ;;

  on)
    # 1) doze 白名单：深度 doze 那一套（冻 + 掐网）靠它
    if ! whitelisted; then
      "$ADB" shell "dumpsys deviceidle whitelist +$PKG" >/dev/null
    fi
    # 2) 叫到前台：**这一步治「失去焦点被冻」**。锁屏挡在前面也没关系 ——
    #    只要焦点还在我们身上，freezer 就不碰它（熄屏 Dozing 下也稳）。
    #    已经在前台时 am start 只是把 task 提到前面，无副作用（而且它自己会
    #    报「intent has been delivered to currently running top-most instance」）。
    "$ADB" shell "am start -n $ICON" >/dev/null 2>&1 || true
    sleep 2
    report
    ;;

  off)
    "$ADB" shell "dumpsys deviceidle whitelist -$PKG" >/dev/null
    echo "已从 doze 白名单移除（「叫到前台」这件事不管了：App 一失去焦点，60~75 秒后又会被冻）"
    ;;

  *)
    sed -n '3,42p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
