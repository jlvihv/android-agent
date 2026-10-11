#!/usr/bin/env bash
#
# 卸载重装 —— 签名对不上时唯一的出路。
#
# 什么时候需要：`adb install -r` 报 INSTALL_FAILED_UPDATE_INCOMPATIBLE。
# 手机上那个包是**另一台机器**的 debug keystore 签的（都是 CN=Android Debug，
# 但证书指纹不同），Android 不允许换签名，只能卸了重装。
#
# 这个脚本先备份**不可再生的**东西，装完再放回去：
#
#   files/sessions.db   对话历史（唯一真正不可再生的）
#   files/llm.json      LLM 配置
#
# 不备份（都会自己回来 / 本来就该重来）：
#   files/alpine-rootfs   沙箱根目录 —— 装完 App 从 assets 自己解压。
#                         所以之前 agent 写在 `/root` 里的东西会没；
#                         写在 /sdcard/Agent 里的是用户存储，不受影响
#   /sdcard/Agent         卸载本来就不动它
#
# 权限：卸载会清掉运行时权限，所以装完顺手把「所有文件访问」用 appops 授回去；
# 无障碍服务由 `just restart` 那套（dev_a11y_on）重挂。
#
# 用法：
#   scripts/phone-reinstall.sh
#   BACKUP=.dev/phone-backup APK=.../app-release.apk scripts/phone-reinstall.sh
set -euo pipefail

. "$(dirname "$0")/adb.sh"

PKG=dev.vihv.agent
APK="${APK:-android/app/build/outputs/apk/debug/app-debug.apk}"
BACKUP="${BACKUP:-.dev/phone-backup}"

[ -f "$APK" ] || { echo "找不到 APK: $APK（先 just android-debug）" >&2; exit 1; }

dev=$("$ADB" devices | awk '$2 == "device" { print $1; exit }')
if [ -z "${dev:-}" ]; then
  echo "没有连着的设备。先 just wifi（或 just wifi-tcpip）" >&2
  exit 1
fi
export ANDROID_SERIAL="$dev"

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }

say "0. 停掉 App（让 SQLite 静下来）"
"$ADB" shell am force-stop "$PKG" >/dev/null 2>&1 || true
sleep 1

say "1. 备份数据 → $BACKUP"
mkdir -p "$BACKUP"
for f in sessions.db sessions.db-wal sessions.db-shm llm.json; do
  "$ADB" shell "run-as $PKG cat files/$f" > "$BACKUP/$f" 2>/dev/null || true
  printf '   %-18s %9s bytes\n' "$f" "$(stat -c%s "$BACKUP/$f" 2>/dev/null || echo 0)"
done

say "2. 校验备份（坏了就中止，不动设备上的东西）"
if command -v sqlite3 >/dev/null 2>&1; then
  # WAL 并进主库，得到一个自洽的单文件
  sqlite3 "$BACKUP/sessions.db" 'PRAGMA wal_checkpoint(TRUNCATE);' >/dev/null
  ok=$(sqlite3 "$BACKUP/sessions.db" 'PRAGMA integrity_check;')
  n=$(sqlite3 "$BACKUP/sessions.db" 'select count(*) from sessions;')
  m=$(sqlite3 "$BACKUP/sessions.db" 'select count(*) from messages;')
  echo "   integrity=$ok · $n 个会话 · $m 条消息"
  if [ "$ok" != "ok" ] || [ "${n:-0}" -lt 1 ]; then
    echo "备份不完整，中止。" >&2
    exit 1
  fi
else
  echo "   （没有 sqlite3，跳过校验 —— 装完记得看一眼会话还在不在）"
fi

say "3. 卸载"
"$ADB" uninstall "$PKG"

say "4. 安装 $APK"
"$ADB" install "$APK"

say "5. 起一次 App（建 files/ 并让 rootfs 开始解压）"
"$ADB" shell am start -n "$PKG/.MainActivity" >/dev/null
sleep 6
"$ADB" shell am force-stop "$PKG" >/dev/null 2>&1 || true
sleep 1

say "6. 放回数据"
put() { # $1 = 备份里的文件名，$2 = 设备上的目标名
  local src="$BACKUP/$1" dst="$2"
  [ -s "$src" ] || return 0
  "$ADB" push "$src" /data/local/tmp/agent-restore >/dev/null
  # /data/local/tmp 是 shell 的目录，app uid 读不了 0600 的文件
  "$ADB" shell chmod 644 /data/local/tmp/agent-restore
  "$ADB" shell "run-as $PKG sh -c 'cat /data/local/tmp/agent-restore > files/$dst'"
  "$ADB" shell rm -f /data/local/tmp/agent-restore
  printf '   %s\n' "$dst"
}
if [ -s "$BACKUP/sessions.db" ]; then
  "$ADB" shell "run-as $PKG sh -c 'rm -f files/sessions.db-wal files/sessions.db-shm'"
  put sessions.db sessions.db
fi
put llm.json llm.json

say "7. 把「所有文件访问」授回去（卸载会清掉运行时权限）"
"$ADB" shell appops set "$PKG" MANAGE_EXTERNAL_STORAGE allow 2>/dev/null || \
  echo "   appops 没成功 —— 到设置里手动开一下「所有文件访问权限」"

say "8. 重启 App"
"$ADB" shell am force-stop "$PKG" >/dev/null 2>&1 || true
"$ADB" shell am start -n "$PKG/.MainActivity" >/dev/null
sleep 8

echo
echo "好了。接着跑："
echo "  just restart     # 重挂无障碍服务 + 转发端口（force-stop 会解绑它）"
echo "  just doctor"
