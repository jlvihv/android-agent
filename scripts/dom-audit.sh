#!/usr/bin/env bash
#
# 全本地跑一次 DOM 审计 —— 不碰手机、不碰 adb、不花 token。
#
# 起四个进程，跑完自动收：
#
#   假 LLM   :8790   scripts/fixture-llm.ts        —— 给界面提供**受控内容**
#   核心     :8791   cargo run --bin agent-dev     —— 独立实例，LLM 走环境变量（只在内存里，
#                                                    不读不写你的 sessions.db，也不动 8787 那个）
#   vite     :5173   web/                          —— 代理 /api 到 8791；改前端源码不用重编 Rust
#   Chrome   :9222   headless + CDP                —— 真正被审计的那个渲染树
#
# 为什么用 vite 而不是核心自己 serve 的 dist：前端是 rust-embed **编译期**嵌进 .so 的，
# 走核心那条路每改一行前端都要重编 Rust（几十秒）。审计是「改完就看」，必须走 vite。
#
# 用法：
#     scripts/dom-audit.sh                 # 全量审计
#     scripts/dom-audit.sh --no-panels     # 只看会话区，不点那些面板
#     scripts/dom-audit.sh --snapshot /tmp/a.json   # 导几何快照（改前端前后对比用）
#     scripts/dom-audit.sh --diff /tmp/a.json
#
# 端口可以用环境变量换：FIXTURE_PORT / CORE_PORT / VITE_PORT / CDP_PORT。
set -euo pipefail
cd "$(dirname "$0")/.."

# 端口可以用环境变量固定；不设就自动挑一个空闲的（从下面这些基数往上找）。
# 为什么必须自动挑：dev 机器上常年躺着一个 5173 的 vite（上一轮开发忘了关），
# 写死端口会直接撞上去。
BASE_FIXTURE_PORT="${FIXTURE_PORT:-8790}"
BASE_CORE_PORT="${CORE_PORT:-8791}"
BASE_VITE_PORT="${VITE_PORT:-5173}"
BASE_CDP_PORT="${CDP_PORT:-9222}"

# 从 $1 开始找第一个没人在听的端口
free_port() {
  local p="$1"
  while (exec 3<"/dev/tcp/127.0.0.1/$p") 2>/dev/null; do
    exec 3<&- 2>/dev/null || true
    p=$((p + 1))
  done
  echo "$p"
}

FIXTURE_PORT="$([ -n "${FIXTURE_PORT:-}" ] && echo "$FIXTURE_PORT" || free_port "$BASE_FIXTURE_PORT")"
CORE_PORT="$([ -n "${CORE_PORT:-}" ] && echo "$CORE_PORT" || free_port "$BASE_CORE_PORT")"
VITE_PORT="$([ -n "${VITE_PORT:-}" ] && echo "$VITE_PORT" || free_port "$BASE_VITE_PORT")"
CDP_PORT="$([ -n "${CDP_PORT:-}" ] && echo "$CDP_PORT" || free_port "$BASE_CDP_PORT")"

# —— 找一份 Chrome ——
find_chrome() {
  for c in "${CHROME:-}" google-chrome-stable google-chrome chromium chromium-browser; do
    [ -n "$c" ] && command -v "$c" >/dev/null 2>&1 && { command -v "$c"; return; }
  done
  # playwright 装的那份
  for p in "$HOME"/.cache/ms-playwright/chromium-*/chrome-linux/chrome; do
    [ -x "$p" ] && { echo "$p"; return; }
  done
  echo "找不到 Chrome。装一个，或者设 CHROME=/path/to/chrome" >&2
  exit 1
}

# —— 等一个 HTTP 端点起来 ——
wait_http() { # $1=url $2=名字 $3=最多几秒
  local i=0
  until curl -sf -m 2 -o /dev/null "$1"; do
    i=$((i + 1))
    [ "$i" -ge "$3" ] && { echo "$2 没起来（$1）" >&2; return 1; }
    sleep 1
  done
}

PIDS=()
cleanup() {
  for pid in "${PIDS[@]:-}"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo "→ 假 LLM :$FIXTURE_PORT（自动挑的空闲端口，避免踩到别人）"
FIXTURE_PORT="$FIXTURE_PORT" bun scripts/fixture-llm.ts >/tmp/dom-audit-fixture.log 2>&1 &
PIDS+=($!)
wait_http "http://127.0.0.1:$FIXTURE_PORT/v1/models" "假 LLM" 20

# 前端得先有一份 dist：rust-embed 是**编译期**嵌的，目录不存在核心编不过
if [ ! -d web/dist ]; then
  echo "→ web/dist 不存在，先构建一份（核心编译期要用）"
  (cd web && bun install --frozen-lockfile && bun run build)
fi

echo "→ 核心 :$CORE_PORT（独立实例，LLM 走环境变量，只在内存里）"
# Linux 沙箱：夹具第一轮会发一个真的 bash 工具调用，要真跑就得有沙箱。
# 没装（没跑过 `just linux-setup`）也能跑，只是那个工具调用会报错、工具行长得难看。
SANDBOX_ROOTFS="${AGENT_LINUX_ROOTFS:-$PWD/.dev/linux/rootfs}"
if [ ! -d "$SANDBOX_ROOTFS" ]; then
  echo "  ⚠️ 没找到 rootfs（$SANDBOX_ROOTFS）—— 夹具那个 bash 调用会报错。"
  echo "     想要干净的结果：先跑 just linux-setup"
fi
AGENT_PORT="$CORE_PORT" \
AGENT_LLM_BASE_URL="http://127.0.0.1:$FIXTURE_PORT/v1" \
AGENT_LLM_API_KEY=fixture \
AGENT_LLM_MODEL=fixture \
AGENT_LLM_API_STYLE=chat \
AGENT_LLM_NAME=fixture \
AGENT_LINUX_ROOTFS="$SANDBOX_ROOTFS" \
AGENT_LINUX_PROOT="${AGENT_LINUX_PROOT:-$(command -v proot || echo proot)}" \
  cargo run --quiet --manifest-path core/Cargo.toml --bin agent-dev >/tmp/dom-audit-core.log 2>&1 &
PIDS+=($!)
wait_http "http://127.0.0.1:$CORE_PORT/api/health" "核心" 300

echo "→ vite :$VITE_PORT（代理 /api → :$CORE_PORT）"
(cd web && AGENT_CORE="http://127.0.0.1:$CORE_PORT" bun run dev --port "$VITE_PORT" --strictPort) \
  >/tmp/dom-audit-vite.log 2>&1 &
PIDS+=($!)
wait_http "http://127.0.0.1:$VITE_PORT/" "vite" 60

echo "→ headless Chrome（CDP :$CDP_PORT）"
CHROME_DIR="$(mktemp -d)"
"$(find_chrome)" --headless=new --remote-debugging-port="$CDP_PORT" --user-data-dir="$CHROME_DIR" \
  --no-first-run --no-default-browser-check --disable-gpu about:blank \
  >/tmp/dom-audit-chrome.log 2>&1 &
PIDS+=($!)
wait_http "http://127.0.0.1:$CDP_PORT/json/version" "Chrome" 30

echo
bun scripts/dom-audit.ts --base "http://127.0.0.1:$VITE_PORT" --cdp "http://127.0.0.1:$CDP_PORT" "$@"
