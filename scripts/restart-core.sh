#!/usr/bin/env bash
#
# 重启电脑上那个核心（宿主模式）——包括「你正在跟我说话，而我跑在那个核心里」这种情况。
#
# 为什么需要它：核心改了代码要重启才生效，而重启会**掐掉正在跑的回合**（也就是正在跟你
# 说话的那个 agent）。所以顺序很讲究：
#
#   1. 先把新二进制编出来 —— **编不动就什么都不做**，老核心照旧跑着
#   2. 等核心空闲（`/api/session` 的 `busy=false`）：等你那句话说答完、消息已经发到手机上
#   3. 才杀老的（连同它的两个子进程：mDNS 广播、无头 Chrome）
#   4. 原样起新的（复刻老核心的环境变量 + cwd），验健康；起不来就用备份的旧二进制兜回去
#   5. 顺手把手机上的 App 重开一次 —— 免得界面停在旧连接上
#
# 从 agent 的 bash 里跑它会**自己脱离会话**（setsid）：agent 的进程树挂在被杀的核心里，
# 不脱离会被一起收走（而且连"半路失败"都看不到）。从你自己的终端跑就不用，输出直接看。
#
# 重启之后手机**不用重新配对**：配对时给手机的凭据就是核心自己的 token，它在会话库里，
# 跨重启不变（换的只是新设备配对用的那个 6 位码）。
#
# 用法：
#   scripts/restart-core.sh              # 编 + 等空闲 + 重启（从哪儿跑都行）
#   scripts/restart-core.sh --dry-run    # 只说它打算干什么，不动手
#   scripts/restart-core.sh --now        # 不等空闲，立刻重启（会掐掉正在跑的回合）
#   scripts/restart-core.sh --no-build   # 不重新编译（用当前二进制重启）
#   scripts/restart-core.sh --no-phone   # 不碰手机上的 App
#
# 日志：.dev/core-restart.log（追加）
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/android-agent"
LOCK="$DATA_DIR/core.lock"
LOG="$ROOT/.dev/core-restart.log"
BACKUP="$ROOT/.dev/agent-dev.prev"
BIN="$ROOT/core/target/release/agent-dev"
HEALTH_TRIES=60

mkdir -p "$ROOT/.dev"
log() { printf '[%s] %s\n' "$(date '+%m-%d %H:%M:%S')" "$*" >>"$LOG"; }

DRY=0
NOW=0
BUILD=1
PHONE=1
for a in "$@"; do
    case "$a" in
        --dry-run) DRY=1 ;;
        --now) NOW=1 ;;
        --no-build) BUILD=0 ;;
        --no-phone) PHONE=0 ;;
        -h | --help) sed -n '2,32p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)
            echo "未知参数: $a（--help 看用法）" >&2
            exit 2
            ;;
    esac
done

# --- 找到现在跑着的核心 -----------------------------------------------------
# 首选锁文件（`single.rs` 写进去的「谁拿着锁」，就是为这种场景留的）
core_pid() {
    python3 - "$LOCK" <<'PY' 2>/dev/null
import json, sys
try:
    print(json.load(open(sys.argv[1]))["pid"])
except Exception:
    pass
PY
}
PID="$(core_pid || true)"
if [ -z "${PID:-}" ] || ! kill -0 "$PID" 2>/dev/null; then
    # 锁里没写 / 写着的人已经没了：按进程名兜底（`[-]` 免得匹配到自己这条命令行）
    PID="$(pgrep -f 'release/agent[-]dev' | head -1 || true)"
fi
if [ -z "${PID:-}" ]; then
    echo "没找到跑着的核心 —— 直接起一个就行（比如 just dev-host）。" >&2
    exit 1
fi

# 端口 / 环境 / cwd 都从那个活着的进程上抄，起回来才是同一个东西
PORT="$(tr '\0' '\n' <"/proc/$PID/environ" | sed -n 's/^AGENT_PORT=//p' | head -1)"
PORT="${PORT:-8787}"
CWD="$(readlink "/proc/$PID/cwd" || echo "$ROOT")"
ENVFILE="$ROOT/.dev/core-restart.env"
tr '\0' '\n' <"/proc/$PID/environ" >"$ENVFILE"
# 老核心自己给的凭据（没有就说明 loopback 免登录，本机请求不用带）
TOKEN="$(sed -n 's/^AGENT_TOKEN=//p' "$ENVFILE" | head -1)"
BASE="http://127.0.0.1:$PORT"
CURL=(curl -s --max-time 3)
[ -n "$TOKEN" ] && CURL+=(-H "Cookie: agent_token=$TOKEN")

# 有没有回合在跑。看 `/api/debug/state` 的 `busySessions`（**别用 `/api/session`**：
# 那个要把整段会话都吐出来，一秒钟拉一遍太浪费；而且 `curl | grep -q` 早退会让 curl
# 吃 SIGPIPE —— 配上 `set -o pipefail` 就是「管道非零」，于是 busy() 永远说空闲，
# 脚本会在回合还没答完时就把核心杀掉（差点就这么干了）。
busy() {
    local s
    s="$("${CURL[@]}" "$BASE/api/debug/state" 2>/dev/null | tr -d ' \n' || true)"
    case "$s" in
        *'"busySessions":[]'*) return 1 ;; # 空数组 = 没有回合在跑
        *'"busySessions":['*) return 0 ;;  # 有
        *) return 1 ;;                     # 查不到就当空闲，后面还有健康检查兜
    esac
}

# --- 从 agent 的进程树里跑？自己脱离会话再来一遍 -----------------------------
ppid_of() { sed 's/.*) //' "/proc/$1/stat" 2>/dev/null | awk '{print $2}'; }
inside_core() {
    local p=$PPID
    while [ -n "${p:-}" ] && [ "$p" -gt 1 ]; do
        [ "$p" = "$PID" ] && return 0
        p="$(ppid_of "$p" || true)"
    done
    return 1
}

if [ "$DRY" = 0 ] && [ -z "${AGENT_RESTART_DETACHED:-}" ] && inside_core; then
    log "从 agent 的进程树里跑（核心 pid=$PID）—— 用 setsid 脱离会话重跑"
    AGENT_RESTART_DETACHED=1 setsid "$0" "$@" >>"$LOG" 2>&1 </dev/null &
    echo "已在后台跑（脱离会话，日志：$LOG）"
    exit 0
fi

log "=== restart-core: pid=$PID port=$PORT cwd=$CWD ==="
log "参数: build=$BUILD now=$NOW phone=$PHONE dry=$DRY"

if [ "$DRY" = 1 ]; then
    echo "核心:      pid=$PID  端口=$PORT  cwd=$CWD"
    echo "二进制:    $BIN"
    echo "环境变量:  从 /proc/$PID/environ 抄（已存到 $ENVFILE）"
    echo "手机 App:  $([ "$PHONE" = 1 ] && echo "会用 adb 重开" || echo "不动")"
    if [ "$NOW" = 1 ]; then
        echo "空闲等待:  跳过（--now）"
    elif busy; then
        echo "空闲等待:  **现在有回合在跑**，会等它结束再动手"
    else
        echo "空闲等待:  现在空闲，编译完就动手"
    fi
    [ "$BUILD" = 1 ] && echo "会先编译:  cargo build --release --bin agent-dev --manifest-path core/Cargo.toml（编不动就什么都不做）"
    echo "（--dry-run：什么都没动）"
    exit 0
fi

# 一次只跑一个重启
exec 9>"$ROOT/.dev/core-restart.lock"
flock -n 9 || {
    log "已经有一个重启在跑了，退出"
    exit 0
}

# --- 1. 备份 + 编译（先编译，失败什么都不动） -------------------------------
cp -f "$BIN" "$BACKUP" 2>/dev/null || true
if [ "$BUILD" = 1 ]; then
    log "编译中：cargo build --release --bin agent-dev（core/Cargo.toml）"
    # --manifest-path 不能少：仓库根没有 Cargo.toml（那个在 core/ 下），
    # 输出照样落在 core/target/release/，和 $BIN 对得上
    if ! cargo build --release --bin agent-dev --manifest-path core/Cargo.toml >>"$LOG" 2>&1; then
        log "❌ 编译失败 —— 什么都不动，老核心（pid=$PID）照旧跑着"
        exit 1
    fi
    log "编译完成"
fi

# --- 2. 等空闲 --------------------------------------------------------------
if [ "$NOW" = 0 ]; then
    waited=0
    while busy; do
        [ "$waited" = 0 ] && log "有回合在跑，等它结束…"
        sleep 1
        waited=$((waited + 1))
        if [ "$waited" -ge 900 ]; then
            log "等了 15 分钟还在忙，放弃（下次再来）"
            exit 1
        fi
    done
    [ "$waited" -gt 0 ] && log "空闲了（等了 ${waited}s）"
    # 让最后那条消息**落盘**：回合结束时只打了个 dirty 标记，库是每 2 秒写一次
    # （`AppState::sessions_dirty` 那个定时器）。留 5 秒，重启后新的我才能看到完整历史。
    sleep 5
fi

# 健康检查：两条路（systemd / 手动）都要用，所以放前面
healthy() {
    for _ in $(seq 1 "$HEALTH_TRIES"); do
        if [ "$("${CURL[@]}" -o /dev/null -w '%{http_code}' "$BASE/api/health" 2>/dev/null)" = "200" ]; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# 这个核心归 systemd 用户服务管吗？（`android-agent.service`，开机自启那套）
#
# 归它管就**只**用 systemctl 重启：手动 kill + 手动起会和它抢端口、抢单实例锁 ——
# 它有 `Restart=always`，会先把旧的核心拉回来，我们手动起的那个死在锁上，脚本
# 还会以为「新核心没起来」，拿备份的旧二进制兜回去（越兜越乱）。
# 想让脚本自己起（比如临时试个别的二进制）：先 `systemctl --user stop android-agent`。
SYSTEMD_UNIT=android-agent.service
SYSTEMD_MANAGED=0
if systemctl --user is-active --quiet "$SYSTEMD_UNIT" 2>/dev/null &&
    [ "$(systemctl --user show -p MainPID --value "$SYSTEMD_UNIT" 2>/dev/null)" = "$PID" ]; then
    SYSTEMD_MANAGED=1
fi

if [ "$SYSTEMD_MANAGED" = 1 ]; then
    # --- 3/4. 重启：交给 systemd（环境在 unit 里，不抄 /proc） ---------------
    log "核心归 $SYSTEMD_UNIT 管 —— systemctl --user restart（不手动 kill / 起）"
    systemctl --user restart "$SYSTEMD_UNIT"
    if healthy; then
        log "✅ 新核心起来了（systemctl --user restart，$BASE）"
    else
        log "❌ systemctl --user restart 之后健康检查一直不过 —— journalctl --user -u $SYSTEMD_UNIT"
        exit 1
    fi
else
    # --- 3. 杀老核心（连它的子进程：mDNS 广播、无头 Chrome） -----------------
    log "杀掉核心 pid=$PID"
    kill "$PID" 2>/dev/null || true
    for _ in $(seq 1 50); do
        kill -0 "$PID" 2>/dev/null || break
        sleep 0.2
    done
    kill -9 "$PID" 2>/dev/null || true
    # 旧广播留着会和新核心发的那条撞名；旧 Chrome 不解掉会让新核心起不了它自己那只
    # （CDP 那只共用同一个 user-data-dir，有单实例锁）
    pkill -f 'avahi-publish-service' 2>/dev/null || true
    pkill -f "user-data-dir=$DATA_DIR/chrome" 2>/dev/null || true
    sleep 1

    # --- 4. 原样起新的 ------------------------------------------------------
    start_core() { # $1 = 用哪个二进制
        local bin="$1"
        log "启动 $bin"
        # 末尾的 `9>&-` 不能少：fd 9 是上面那把「一次只跑一个重启」的 flock（`exec 9>…`）。
        # 子进程会**继承**它，而这个子进程最后 execve 成了新核心 —— 于是锁被跑着的核心
        # 一直攥着，**下一次**重启走到 `flock -n 9` 就悄悄说一句「已经有一个重启在跑了」
        # 退出 0：什么都没做，看着却像成功了（核心的 fd 9 指着 core-restart.lock，
        # 连它拉起来的 avahi 也跟着攥着）。
        AGENT_RESTART_BIN="$bin" AGENT_RESTART_CWD="$CWD" AGENT_RESTART_ENV="$ENVFILE" \
            setsid python3 - <<'PY' >>"$LOG" 2>&1 9>&- &
import os
bin = os.environ["AGENT_RESTART_BIN"]
cwd = os.environ["AGENT_RESTART_CWD"]
env = {}
for line in open(os.environ["AGENT_RESTART_ENV"], encoding="utf-8", errors="replace"):
    line = line.rstrip("\n")
    if "=" in line:
        k, v = line.split("=", 1)
        env[k] = v
env.pop("AGENT_MULTI", None)   # 单实例锁该拿还得拿
os.chdir(cwd)
os.execve(bin, [bin], env)
PY
    }

    # 起一把不行就再试：老核心刚死时那个端口可能还没放干净（TIME_WAIT 之类），
    # 一次失败就退到备份二进制太急了。
    try_start() { # $1 = 二进制，$2 = 叫什么（日志里用）
        local bin="$1" what="$2" attempt
        for attempt in 1 2 3; do
            start_core "$bin"
            if healthy; then
                log "✅ $what 起来了（$BASE，第 $attempt 次尝试）"
                return 0
            fi
            log "⚠️ $what 第 $attempt 次没起来（端口没放干净？）—— 清一下、5 秒后再试"
            pkill -f 'release/agent-[d]ev' 2>/dev/null || true
            pkill -f 'agent-dev[.]prev' 2>/dev/null || true
            sleep 5
        done
        return 1
    }

    if ! try_start "$BIN" "新核心"; then
        log "❌ 新核心三次都没起来 —— 用备份的旧二进制兜回去（$BACKUP）"
        if try_start "$BACKUP" "旧的（备份）"; then
            log "⚠️ 退回到旧二进制了：现在是能用的状态，但跑的是老代码"
        else
            log "❌❌ 都起不来 —— 到终端里手动跑：just dev-host"
            exit 1
        fi
    fi
fi

# --- 5. 把手机上的 App 重开一次（免得界面停在旧连接上） ----------------------
if [ "$PHONE" = 1 ]; then
    . "$ROOT/scripts/adb.sh"
    DEV="$("$ADB" devices 2>/dev/null | awk '$2 == "device" && $1 !~ /^emulator-/ { print $1; exit }')"
    if [ -n "${DEV:-}" ]; then
        "$ADB" -s "$DEV" shell am force-stop dev.vihv.agent >/dev/null 2>&1 || true
        sleep 1
        "$ADB" -s "$DEV" shell am start -n dev.vihv.agent/.MainActivity >/dev/null 2>&1 || true
        log "手机 App 已重开（$DEV）—— 接着发消息就行"
    else
        log "没看到手机（adb），没动 App —— 手动重开一下就是"
    fi
fi
log "=== 完成 ==="
