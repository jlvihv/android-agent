#!/usr/bin/env bash
#
# 重启电脑上那个核心（改完 Rust 代码要生效时用它）。
#
# 顺序讲究：**先编译**（编不动就什么都不做，老核心照旧跑着）→ **等空闲**（别掐掉正在
# 跑的回合，也就是正在跟你说话的那个）→ 让系统服务重启它 → 验健康。
#
# 核心归 launchd（macOS）/ systemd 用户服务（Linux）管时**只**走它们，不手动 kill / 起：
# 手动那套会和它抢端口、抢单实例锁（它有 KeepAlive / Restart=always，会先把旧的核心
# 拉回来）。没装服务就先 `just service install`。
#
# 从 agent 的 bash 里跑会自己脱离会话（setsid）：agent 的进程树挂在被重启的核心里，
# 不脱离会被一起收走，连「半路失败」都看不到。
#
# 用法：
#   scripts/restart-core.sh              # 编 + 等空闲 + 重启
#   scripts/restart-core.sh --dry-run    # 只说打算干什么
#   scripts/restart-core.sh --now        # 不等空闲，立刻重启（会掐掉正在跑的回合）
#   scripts/restart-core.sh --no-build   # 不重新编译
#
# 日志：.dev/core-restart.log（追加）
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/android-agent"
LOCK="$DATA_DIR/core.lock"
LOG="$ROOT/.dev/core-restart.log"
LABEL=dev.vihv.android-agent
UNIT=android-agent.service

mkdir -p "$ROOT/.dev"
log() { printf '[%s] %s\n' "$(date '+%m-%d %H:%M:%S')" "$*" >>"$LOG"; }

DRY=0
NOW=0
BUILD=1
for a in "$@"; do
    case "$a" in
        --dry-run) DRY=1 ;;
        --now) NOW=1 ;;
        --no-build) BUILD=0 ;;
        -h | --help)
            sed -n '2,22p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "未知参数: $a（--help 看用法）" >&2
            exit 2
            ;;
    esac
done

# --- 谁在跑、听哪个端口：锁文件里写着（`single.rs` 写进去的，就为这种场景） ----
read -r PID PORT < <(python3 -c '
import json, sys
try:
    d = json.load(open(sys.argv[1]))
    print(d.get("pid", 0), d.get("port", 0))
except Exception:
    print(0, 0)
' "$LOCK")
[ "${PORT:-0}" = 0 ] && PORT="${AGENT_PORT:-8787}"
BASE="http://127.0.0.1:$PORT"

# --- 凭据 -------------------------------------------------------------------
# 本机要不要凭据由**核心自己**的环境决定（main.rs 的 `trust_loopback`）：它的环境里
# 有 AGENT_TOKEN（systemd 用户管理器里常常被导进去一把）时，连 127.0.0.1 都要带。
# 而脚本自己那份环境往往没有那一把 —— 那就 401，健康检查永远不过（实测踩到）。
# 所以这里主动去找，顺序和核心取 token 的顺序一致：
#   $AGENT_TOKEN → systemd 用户环境 → 库里那把（sessions.db 的 meta.token）
# 都没有就不带：那说明核心本机免登录（`(None, false)` 那条）。
db_token() {
    python3 - "$DATA_DIR/data/sessions.db" <<'PY' 2>/dev/null || true
import sqlite3, sys
try:
    c = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True, timeout=2)
    v = c.execute("SELECT v FROM meta WHERE k='token'").fetchone()
    print(v[0] if v and isinstance(v[0], str) else "")
except Exception:
    pass
PY
}

TOKEN="${AGENT_TOKEN:-}"
TOKEN_SRC='$AGENT_TOKEN'
if [ -z "$TOKEN" ] && command -v systemctl >/dev/null 2>&1; then
    TOKEN="$(systemctl --user show-environment 2>/dev/null | sed -n 's/^AGENT_TOKEN=//p' | head -1 || true)"
    [ -n "$TOKEN" ] && TOKEN_SRC="systemd 用户环境"
fi
if [ -z "$TOKEN" ]; then
    TOKEN="$(db_token)"
    [ -n "$TOKEN" ] && TOKEN_SRC="sessions.db"
fi
[ -z "$TOKEN" ] && TOKEN_SRC="无（核心本机免登录）"

CURL=(curl -s --max-time 3)
if [ -n "$TOKEN" ]; then CURL+=(-H "Cookie: agent_token=$TOKEN"); fi

# 只看状态码：连着查询、401、连不上，得能分清楚（401 = 核心活着但脚本没凭据，
# 别傻等 60 秒；连不上 = 还在起 / 起不来，那才值得等）。
code() {
    local c
    c="$("${CURL[@]}" -o /dev/null -w '%{http_code}' "$BASE$1" 2>/dev/null || true)"
    case "$c" in [0-9][0-9][0-9]) echo "$c" ;; *) echo 000 ;; esac
}

# 有没有回合在跑：看 `/api/debug/state` 的 `busySessions`。
# **别用 `/api/session`**：那个要把整段会话吐出来，一秒钟拉一遍太浪费。
# 0 = 忙，1 = 空闲 / 核心没在跑，2 = 核心要凭据而脚本没拿到（查不了，别瞎重启）
busy() {
    local out c s
    out="$("${CURL[@]}" -w '\n%{http_code}' "$BASE/api/debug/state" 2>/dev/null || true)"
    c="${out##*$'\n'}"
    case "$c" in
        200) ;;
        401 | 403) return 2 ;;
        *) return 1 ;;
    esac
    s="$(printf '%s' "${out%$'\n'*}" | tr -d ' \n')"
    case "$s" in
        *'"busySessions":[]'*) return 1 ;; # 空数组 = 空闲
        *'"busySessions":['*) return 0 ;;  # 有
        *) return 1 ;;                     # 看不懂就当空闲，后面还有健康检查兜
    esac
}

need_token_msg() {
    echo "❌ 核心要凭据，脚本拿到的 token 不对（凭据来源：$TOKEN_SRC）。" >&2
    echo "   查一把：systemctl --user show-environment | grep AGENT_TOKEN" >&2
    echo "   或手动指定：AGENT_TOKEN=… $0" >&2
}

# --- 归谁管 -----------------------------------------------------------------
OS="$(uname)"
MANAGED=""
if [ "$OS" = Darwin ] && launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
    MANAGED=launchd
elif [ "$OS" = Linux ] && systemctl --user is-active --quiet "$UNIT" 2>/dev/null; then
    MANAGED=systemd
fi
if [ -z "$MANAGED" ]; then
    echo "核心不是系统服务管的（大概是终端里跑的 just dev）—— 那种我重启不了：" >&2
    echo "  要么去那个终端 Ctrl-C 再起；要么先把它装成服务：just service install" >&2
    exit 1
fi

# --- 从 agent 的进程树里跑？自己脱离会话再来一遍 -----------------------------
ppid_of() { ps -o ppid= -p "$1" 2>/dev/null | tr -d ' '; }
# 绝对路径：重跑时工作目录不一定是仓库根（systemd-run 起的那份 cwd 是 $HOME）
SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
inside_core() {
    local p=$PPID
    while [ -n "${p:-}" ] && [ "$p" -gt 1 ]; do
        [ "$p" = "$PID" ] && return 0
        p="$(ppid_of "$p" || true)"
    done
    return 1
}
if [ "$DRY" = 0 ] && [ -z "${AGENT_RESTART_DETACHED:-}" ] && inside_core; then
    log "从 agent 的进程树里跑（核心 pid=$PID）—— 脱离它的 cgroup 再来一遍"
    # **setsid 不够**：那只换会话，进程还在 android-agent.service 的 cgroup 里，
    # `systemctl --user restart` 会把它一起 SIGTERM 掉 —— 连「失败了」都看不到
    # （实测：日志里 restart 之后什么都没有）。systemd-run 起一个自己的 transient
    # unit 才真脱离；起不来再退回 setsid（至少比什么都不做强）。
    if command -v systemd-run >/dev/null 2>&1 && [ "${MANAGED:-}" = systemd ]; then
        # 把父进程找到的东西显式带过去（transient unit 不继承调用者的环境）
        env_args=(--setenv=AGENT_RESTART_DETACHED=1 --setenv=AGENT_RESTART_LOG="$LOG")
        [ -n "${XDG_DATA_HOME:-}" ] && env_args+=(--setenv=XDG_DATA_HOME="$XDG_DATA_HOME")
        [ -n "$TOKEN" ] && env_args+=(--setenv=AGENT_TOKEN="$TOKEN")
        if systemd-run --user --collect --quiet \
            --unit="agent-restart-core-$$" "${env_args[@]}" \
            /bin/bash -c '"$@" >>"$AGENT_RESTART_LOG" 2>&1' bash "$SELF" "$@"; then
            echo "已在后台跑（独立 unit，日志：$LOG）"
            exit 0
        fi
        log "systemd-run 起不来，退回 setsid（可能会被 restart 一起收走）"
    fi
    AGENT_RESTART_DETACHED=1 setsid "$SELF" "$@" >>"$LOG" 2>&1 </dev/null &
    echo "已在后台跑（脱离会话，日志：$LOG）"
    exit 0
fi

log "=== restart-core: pid=$PID port=$PORT 管=$MANAGED build=$BUILD now=$NOW ==="

if [ "$DRY" = 1 ]; then
    echo "核心:    pid=$PID  端口=$PORT  （$MANAGED 管）"
    echo "二进制:  core/target/release/agent-dev"
    echo "会先编译: $([ "$BUILD" = 1 ] && echo "是（编不动就什么都不做）" || echo "不（--no-build）")"
    echo "空闲等待: $([ "$NOW" = 1 ] && echo "跳过（--now）" || echo "等到 busy=false")"
    echo "重启方式: $([ "$MANAGED" = launchd ] && echo "launchctl kickstart -k $LABEL" || echo "systemctl --user restart $UNIT")"
    echo "凭据:    $TOKEN_SRC"
    echo "（--dry-run：什么都没动）"
    exit 0
fi

# --- 1. 先编译（失败什么都不动） --------------------------------------------
if [ "$BUILD" = 1 ]; then
    log "编译中：cargo build --release --bin agent-dev"
    if ! cargo build --release --bin agent-dev --manifest-path core/Cargo.toml >>"$LOG" 2>&1; then
        log "❌ 编译失败 —— 什么都不动，老核心（pid=$PID）照旧跑着"
        echo "❌ 编译失败，老核心照旧跑着（日志：$LOG）" >&2
        exit 1
    fi
    log "编译完成"
fi

# --- 2. 等空闲（别掐掉正在跑的回合） ----------------------------------------
if [ "$NOW" = 0 ]; then
    waited=0
    while :; do
        rc=0
        busy || rc=$?
        if [ "$rc" = 2 ]; then
            log "❌ /api/debug/state 要凭据（401）—— 查不了回合，不重启（怕掐掉正在跑的）"
            need_token_msg
            exit 1
        fi
        [ "$rc" = 0 ] || break # 空闲（或核心这会儿连不上，交给后面健康检查判）
        [ "$waited" = 0 ] && log "有回合在跑，等它结束…"
        sleep 1
        waited=$((waited + 1))
        if [ "$waited" -ge 900 ]; then
            log "等了 15 分钟还在忙，放弃（下次再来）"
            exit 1
        fi
    done
    if [ "$waited" -gt 0 ]; then
        log "空闲了（等了 ${waited}s）"
        # 让最后那条消息落盘：回合结束只打个 dirty 标记，库是每 2 秒写一次
        sleep 3
    fi
fi

# --- 3. 交给系统服务重启 ----------------------------------------------------
if [ "$MANAGED" = launchd ]; then
    log "launchctl kickstart -k $LABEL"
    launchctl kickstart -k "gui/$(id -u)/$LABEL"
else
    log "systemctl --user restart $UNIT"
    systemctl --user restart "$UNIT"
fi

# --- 4. 验健康 --------------------------------------------------------------
for _ in $(seq 1 60); do
    c="$(code /api/health)"
    case "$c" in
        200)
            log "✅ 新核心起来了（$BASE）"
            echo "✅ 重启完成：$BASE"
            exit 0
            ;;
        401 | 403)
            # 核心在听、也答了 —— 只是脚本没凭据。别再等 60 秒，直接说清楚
            log "❌ 新核心起来了，但 /api/health 要凭据（$c）—— 脚本的 token 不对"
            need_token_msg
            exit 1
            ;;
    esac
    sleep 1
done
log "❌ 重启之后健康检查一直不过（连不上 $BASE）—— 看日志：scripts/service.sh logs"
echo "❌ 重启之后健康检查一直不过 —— 看日志：scripts/service.sh logs" >&2
exit 1
