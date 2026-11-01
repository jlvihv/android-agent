#!/usr/bin/env bash
#
# 把核心装成「开机就在」的常驻服务。
#
#   macOS：launchd 用户代理（`~/Library/LaunchAgents/dev.vihv.android-agent.plist`）
#   Linux：systemd 用户服务（`android-agent.service`）
#
#   scripts/service.sh install     # 装 + 立刻起（macOS 会先要 release 二进制在）
#   scripts/service.sh uninstall   # 停 + 摘掉（不删二进制、不删库）
#   scripts/service.sh status      # 在跑吗、端口、健康
#   scripts/service.sh restart     # 重启（走系统那套，不手动 kill）
#   scripts/service.sh logs        # 跟日志
#
# 为什么用系统服务而不是 nohup：开机自启、崩了自动拉回、日志有地方去。
# 归它管之后 `scripts/restart-core.sh` **只**走 `launchctl kickstart` / `systemctl restart`
# —— 手动 kill + 手动起会和它抢端口、抢单实例锁（它有 KeepAlive / Restart=always，
# 会先把旧的核心拉回来）。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/core/target/release/agent-dev"
LABEL=dev.vihv.android-agent
PLIST_SRC="$ROOT/scripts/launchd/$LABEL.plist"
PLIST_DST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG="$HOME/Library/Logs/android-agent.log"
UNIT=android-agent.service
PORT="${AGENT_PORT:-8787}"
BASE="http://127.0.0.1:$PORT"

case "$(uname)" in
Darwin) OS=macos ;;
Linux) OS=linux ;;
*)
    echo "这套只认 macOS（launchd）和 Linux（systemd）" >&2
    exit 2
    ;;
esac

health() {
    curl -fsS -o /dev/null -m 3 "$BASE/api/health" 2>/dev/null
}

wait_health() {
    for _ in $(seq 1 60); do health && return 0; sleep 1; done
    return 1
}

need_bin() {
    [ -x "$BIN" ] && return 0
    echo "没有 release 二进制：$BIN" >&2
    echo "先编一个：cargo build --release --bin agent-dev --manifest-path core/Cargo.toml" >&2
    echo "（或者直接 scripts/restart-core.sh，它编完会顺手重启）" >&2
    exit 1
}

macos_pid() {
    launchctl print "gui/$(id -u)/$LABEL" 2>/dev/null |
        awk '/^[[:space:]]*pid = /{print $3; exit}'
}

cmd_install() {
    if [ "$OS" = macos ]; then
        need_bin
        mkdir -p "$(dirname "$PLIST_DST")" "$(dirname "$LOG")"
        # 模板里的 __ROOT__ / __HOME__ 换成真实路径（plist 里不能写 shell 变量）
        sed -e "s|__ROOT__|$ROOT|g" -e "s|__HOME__|$HOME|g" "$PLIST_SRC" >"$PLIST_DST"
        # 已经加载过就先摘掉，再装上新的（改了 plist 必须走这一步，launchd 不热读）
        launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
        launchctl bootstrap "gui/$(id -u)" "$PLIST_DST"
        echo "装好了：$PLIST_DST（开机自启，崩了自动拉回）"
    else
        need_bin
        if ! systemctl --user cat "$UNIT" >/dev/null 2>&1; then
            echo "找不到 $UNIT。" >&2
            echo "Linux 上这个 unit 不在仓库里（放在 ~/Code/project/config/systemd/user/），见 docs/06。" >&2
            exit 1
        fi
        systemctl --user enable --now "$UNIT"
        echo "装好了：$UNIT"
    fi
    # 健康检查只看「端口通不通」，分不清是谁在应答：如果这会儿还有人手起的核心
    # （比如 `just dev`），它占着单实例锁，服务里的那个会一直起不来 —— 那要说清楚，
    # 不能让「装好了」看起来像「已经在跑」。
    if [ "$OS" = macos ]; then
        sleep 3
        pid="$(macos_pid || true)"
    else
        pid="$(systemctl --user show -p MainPID --value "$UNIT" 2>/dev/null || true)"
    fi
    if [ -n "${pid:-}" ] && [ "$pid" != "0" ] && wait_health; then
        echo "在跑：$BASE（pid $pid）"
    elif health; then
        echo "⚠️  服务装上了，但它没在跑：$BASE 上是**别的**核心（单实例锁被它占着）。" >&2
        echo "    停掉那个（比如 Ctrl-C 那个 just dev）之后它会在 10 秒内自己接过来；" >&2
        echo "    或者现在换：scripts/service.sh restart" >&2
    else
        echo "⚠️  装上了但 60 秒内没通过健康检查 —— 看日志：scripts/service.sh logs" >&2
        exit 1
    fi
}

cmd_uninstall() {
    if [ "$OS" = macos ]; then
        launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
        rm -f "$PLIST_DST"
        echo "摘掉了（二进制和会话库都没动）"
    else
        systemctl --user disable --now "$UNIT" 2>/dev/null || true
        echo "停掉并取消自启了（二进制和会话库都没动）"
    fi
}

cmd_status() {
    if [ "$OS" = macos ]; then
        local pid
        pid="$(macos_pid || true)"
        if [ -n "${pid:-}" ]; then
            echo "launchd: 在跑（pid $pid）"
        else
            echo "launchd: 没在跑（install 过吗？）"
        fi
        [ -f "$PLIST_DST" ] && echo "plist:    $PLIST_DST"
    else
        systemctl --user status "$UNIT" --no-pager 2>/dev/null | head -5 || echo "没装 $UNIT"
    fi
    if health; then
        echo "健康:     $BASE 200"
        curl -fsS "$BASE/api/health" | python3 -m json.tool 2>/dev/null | head -20 || true
    else
        echo "健康:     $BASE 没响应"
    fi
}

cmd_restart() {
    if [ "$OS" = macos ]; then
        launchctl kickstart -k "gui/$(id -u)/$LABEL"
    else
        systemctl --user restart "$UNIT"
    fi
    if wait_health; then echo "重启完成：$BASE"; else echo "⚠️  重启后没通过健康检查" >&2; exit 1; fi
}

cmd_logs() {
    if [ "$OS" = macos ]; then
        tail -n 50 -f "$LOG"
    else
        journalctl --user -u "$UNIT" -n 50 -f
    fi
}

case "${1:-status}" in
install) cmd_install ;;
uninstall) cmd_uninstall ;;
status) cmd_status ;;
restart) cmd_restart ;;
logs) cmd_logs ;;
-h | --help | help)
    sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    ;;
*)
    echo "未知动作: $1（install / uninstall / status / restart / logs）" >&2
    exit 2
    ;;
esac
