#!/usr/bin/env python3
"""agentctl —— 从电脑上驱动 android-agent 的核心。

核心本来就是个 HTTP 服务（只听 127.0.0.1），所以把它当普通服务驱动就行：

    just phone "在沙箱里建个 fizzbuzz 跑一下"   # 连手机（自动 adb forward）
    just phone                                  # 交互模式（/help 看命令）
    python3 scripts/agentctl.py "..."           # 连本机（just dev 起的那个）
    python3 scripts/agentctl.py --json "..."    # 机器可读的一整份转录

改 Rust 代码仍然要重装 APK（核心是 .so）。但**调提示词、试沙箱、发设备指令、
看下一条请求体**都不用 —— 那些以前只能靠手机屏幕上打字、或者重装一次。

交互模式里 `/help` 列全部命令。几个常用的：

    /state        服务端现在什么情况（一次拿全，不用敲六个接口）
    /request      下一次会发给模型的完整请求体 —— 调提示词 / 查缓存靠它
    /prompt <文字> 运行时换系统提示词（不用重装 APK）
    /bash <命令>   直接进沙箱跑，绕过模型
    /snap         让手机重推一次界面，然后打出来
    /dev {"type":"key","name":"back"}    直接发设备指令
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request

BASE = os.environ.get("AGENT_BASE", "http://127.0.0.1:8787")

# 一个回合的总超时（`--timeout`）。手机上跑长任务时得给够。
TIMEOUT = 1800.0

# 输出到管道（被脚本吃）时不要写颜色和回车刷新
TTY = sys.stdout.isatty()


def c(code: str, s: str) -> str:
    return f"\033[{code}m{s}\033[0m" if TTY else s


DIM = "2"
WARN = "33"
BAD = "31"
OK = "32"


# ---------------------------------------------------------------------------
# HTTP
# ---------------------------------------------------------------------------


def call(method: str, path: str, body=None, timeout: float = 60.0):
    """普通请求。返回 (状态码, 解析后的 body 或原始文本)。"""
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(
        BASE + path,
        data=data,
        method=method,
        headers={"content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            status = r.status
            raw = r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")
    except urllib.error.URLError as e:
        return 0, f"连不上 {BASE}：{e}"
    try:
        return status, json.loads(raw)
    except json.JSONDecodeError:
        return status, raw


def sse(path: str, body, timeout: float = 1800.0):
    """POST 一个 SSE 流，逐个 chunk 产出。"""
    req = urllib.request.Request(
        BASE + path,
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        buf = b""
        while True:
            piece = r.read1(65536)
            if not piece:
                return
            buf += piece
            while b"\n\n" in buf:
                frame, buf = buf.split(b"\n\n", 1)
                for line in frame.decode("utf-8", "replace").split("\n"):
                    if not line.startswith("data:"):
                        continue
                    payload = line[5:].strip()
                    if payload == "[DONE]":
                        return
                    try:
                        yield json.loads(payload)
                    except json.JSONDecodeError:
                        pass


# ---------------------------------------------------------------------------
# 转录：一边流一边打，同时也攒一份机器可读的
# ---------------------------------------------------------------------------


def indent(text: str, limit: int) -> str:
    lines = text.rstrip("\n").split("\n")
    shown = lines[:limit]
    body = "\n".join("   │ " + ln for ln in shown)
    if len(lines) > limit:
        body += f"\n   │ {c(DIM, f'… 还有 {len(lines) - limit} 行')}"
    return body


def compact_args(v) -> str:
    """一行放得下的参数。

    工具参数是一片一片流过来的（`tool-input-delta`），那时已经把原文打出去了；
    但如果服务端直接给 `tool-input-available`（比如演示流），这里补上。
    """
    s = v if isinstance(v, str) else json.dumps(v, ensure_ascii=False)
    return s if len(s) <= 300 else s[:300] + "…"


def fmt_tool_output(name: str, out) -> str:
    if not isinstance(out, dict):
        return "   " + str(out)[:2000]
    if "error" in out:
        return "   " + c(BAD, f"❌ {out['error']}")
    if name == "bash":
        bits = [f"exit={out.get('exit_code')}", f"{out.get('wall_time_seconds', 0):.2f}s"]
        if out.get("truncated"):
            bits.append("已截断")
        if out.get("timed_out"):
            bits.append(c(WARN, "超时"))
        head = "   " + c(OK if out.get("exit_code") == 0 else BAD, " ".join(bits))
        tail = indent(str(out.get("output", "")), 25)
        return f"{head}\n{tail}" if tail.strip(" │") else head
    if name == "edit":
        if out.get("replaced") is not None:
            head = "   " + c(OK, f"已替换 {out['replaced']} 处")
        else:
            head = "   " + c(OK, str(out.get("message", "ok")))
        diff = out.get("diff") or ""
        return f"{head}\n{indent(diff, 30)}" if diff else head
    return "   " + indent(json.dumps(out, ensure_ascii=False), 12)


class Transcript:
    """把 chunk 流折叠成「能看的人话」+「能断言的 JSON」。"""

    def __init__(self, live: bool):
        # live = 要边流边打（--json 时关掉，只出最后那一坨 JSON）。
        # tty = 终端：才做着色和「原地刷新」那套（管道里会乱）。
        self.live = live
        self.tty = TTY
        self.texts: dict[str, str] = {}
        self.reasoning: dict[str, str] = {}
        self.tools: dict[str, dict] = {}
        self.usage: list[dict] = []
        self.errors: list[str] = []
        self.text = ""
        # SSE 断了的话，服务端那份会话（给 --json 用）
        self.server_view: dict | None = None
        # 当前行有没有写过东西（决定换行要不要补）
        self._open = False
        self._stage = ""

    # --- 输出小工具 ---
    def w(self, s: str) -> None:
        if self.live:
            sys.stdout.write(s)
            sys.stdout.flush()
        self._open = True

    def nl(self) -> None:
        if self.live and self._open:
            sys.stdout.write("\n")
            sys.stdout.flush()
        self._open = False

    def line(self, s: str = "") -> None:
        self.nl()
        if self.live and s:
            sys.stdout.write(s + "\n")
            sys.stdout.flush()

    # --- 流 ---
    def feed(self, ch: dict) -> None:
        t = ch.get("type", "")

        if t == "reasoning-start":
            self._stage = "reasoning"
            self.reasoning[ch["id"]] = ""
            self.w(c(DIM, "💭 "))
        elif t == "reasoning-delta":
            self.reasoning[ch["id"]] = self.reasoning.get(ch["id"], "") + ch["delta"]
            self.w(c(DIM, ch["delta"]))
        elif t == "reasoning-end":
            self.nl()

        elif t == "text-start":
            self._stage = "text"
            self.texts[ch["id"]] = ""
        elif t == "text-delta":
            self.texts[ch["id"]] = self.texts.get(ch["id"], "") + ch["delta"]
            self.text = "".join(self.texts.values())
            self.w(ch["delta"])
        elif t == "text-end":
            self.nl()

        elif t == "tool-input-start":
            self.nl()
            self.tools[ch["toolCallId"]] = self._new_tool(ch["toolCallId"], ch["toolName"])
            self.w(f"🔧 {ch['toolName']} ")
        elif t == "tool-input-delta":
            tool = self.tools.get(ch["toolCallId"])
            if tool:
                tool["args_text"] += ch["inputTextDelta"]
                self.w(c(DIM, ch["inputTextDelta"]))
        elif t == "tool-input-available":
            tool = self.tools.setdefault(
                ch["toolCallId"], self._new_tool(ch["toolCallId"], ch["toolName"])
            )
            tool["input"] = ch["input"]
            tool["name"] = ch["toolName"]
            # 参数没流式过来（比如演示流）就在这里补上，否则工具行只有名字
            if self.live and not tool["args_text"]:
                self.w(c(DIM, compact_args(ch["input"])))
        elif t == "tool-output-available":
            tool = self.tools.get(ch["toolCallId"])
            if not tool:
                return
            out = ch.get("output")
            if ch.get("preliminary"):
                # bash 的实时输出：只留最后一行，原地刷新，别刷屏
                last = ""
                if isinstance(out, dict):
                    last = str(out.get("output", "")).strip().split("\n")[-1]
                if self.tty and last:
                    sys.stdout.write("\r   " + c(DIM, f"⏳ {last[:100]}"))
                    sys.stdout.flush()
                    self._open = True
                return
            self.nl()
            tool["output"] = out
            self.line(fmt_tool_output(tool["name"], out))

        elif t == "data-usage":
            self.usage.append(ch["data"])
        elif t == "data-warning":
            self.line("   " + c(WARN, f"⚠ {ch['data'].get('message')}"))
        elif t == "error":
            self.errors.append(ch["errorText"])
            self.nl()
            self.line("   " + c(BAD, f"❌ {ch['errorText']}"))
        elif t in ("start", "finish", "start-step", "reset-step", "abort"):
            self.nl()

    def summary(self) -> str:
        if not self.usage:
            return ""
        u = self.usage[-1]
        cached = None
        if isinstance(u.get("prompt_tokens_details"), dict):
            cached = u["prompt_tokens_details"].get("cached_tokens")
        if cached is None:
            cached = u.get("prompt_cache_hit_tokens")
        prompt = u.get("prompt_tokens", 0)
        reason = None
        if isinstance(u.get("completion_tokens_details"), dict):
            reason = u["completion_tokens_details"].get("reasoning_tokens")
        bits = [f"prompt={prompt}"]
        if cached is not None and prompt:
            bits.append(f"cached={cached}（{cached / prompt * 100:.1f}%）")
        bits.append(f"completion={u.get('completion_tokens', 0)}")
        if reason:
            bits.append(f"reasoning={reason}")
        return c(DIM, "· " + "  ".join(bits))

    def _new_tool(self, cid: str, name: str) -> dict:
        return {"id": cid, "name": name, "args_text": "", "input": None, "output": None}

    def as_json(self) -> dict:
        return {
            "text": self.text,
            "reasoning": "".join(self.reasoning.values()),
            "toolCalls": list(self.tools.values()),
            "usage": self.usage,
            "errors": self.errors,
            "serverView": self.server_view,
        }


# ---------------------------------------------------------------------------
# 一个回合
# ---------------------------------------------------------------------------


def wait_until_idle(timeout: float = 1800.0) -> dict | None:
    """轮询 /api/session 直到服务端不在跑。SSE 断了的时候用它兜底。"""
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        code, d = call("GET", "/api/session")
        if code != 200 or not isinstance(d, dict):
            return None
        last = d
        if not d.get("busy"):
            return d
        time.sleep(1.0)
    return last


def print_server_view(d: dict) -> None:
    """SSE 断了以后，把服务端那份会话的最后一条助手消息打出来。"""
    if not d:
        return
    for m in reversed(d.get("messages", [])):
        if m.get("role") != "assistant":
            continue
        for p in m.get("parts", []):
            if p.get("type") == "text" and p.get("text"):
                print("\n" + c(DIM, "（SSE 断了，这是服务端那份的最终回答）"))
                print(p["text"])
                return


def do_turn(prompt: str, as_json: bool) -> int:
    tr = Transcript(live=not as_json)
    if not as_json:
        print(c(DIM, "── 用户 ──"))
        print(prompt)
        print(c(DIM, "── agent ──"))
    dropped = False
    try:
        for ch in sse("/api/chat", {"text": prompt}, timeout=TIMEOUT):
            tr.feed(ch)
    except Exception as e:  # noqa: BLE001
        dropped = True
        tr.line("   " + c(WARN, f"⚠ 流断了（{e}）—— 服务端可能还在跑，等它"))
    tr.nl()

    if dropped:
        # 手机上 WebView 被切后台、Wi-Fi 掉一下，SSE 都会断，
        # 但服务端的回合不会停。等它跑完再拿服务端那份状态。
        d = wait_until_idle(TIMEOUT)
        tr.server_view = d
        tr.errors.append("stream dropped")
        if not as_json:
            print_server_view(d)

    if as_json:
        print(json.dumps(tr.as_json(), ensure_ascii=False, indent=2))
    else:
        s = tr.summary()
        if s:
            print(s)
    return 1 if tr.errors else 0


# ---------------------------------------------------------------------------
# 交互模式
# ---------------------------------------------------------------------------

HELP = f"""
{c('1', '对话')}
  任意文字                发给 agent
  /wait                  等服务端当前回合跑完（并打出最终回答）

{c('1', '看')}
  /state                 服务端完整状态（/api/debug/state）
  /request [raw]         下一次会发给模型的完整请求体
  /prompt                看当前系统提示词
  /sessions              列会话

{c('1', '改')}
  /prompt <文字>         运行时换系统提示词 —— 不用重装 APK
  /prompt -              从 stdin 读到 EOF（heredoc 好用）
  /prompt!               恢复成编译进去的那版
  /new                   新建会话      /use <id>   切会话     /clear  清空当前会话

{c('1', '直连（绕过模型）')}
  /bash <命令>           在沙箱里跑命令（/api/linux）
  /cat <沙箱路径>        读沙箱里的文件（/sandbox/…）
  /snap                  让手机重推一次界面并打出来
  /dev <json>            发一条设备指令，如 /dev {{"type":"key","name":"back"}}

  /help  /quit
"""


def print_state() -> None:
    code, d = call("GET", "/api/debug/state")
    if code != 200:
        print(c(BAD, f"失败 {code}: {d}"))
        return
    print(json.dumps(d, ensure_ascii=False, indent=2))


def print_request(raw: bool) -> None:
    code, d = call("GET", "/api/debug/request")
    if code != 200:
        print(c(BAD, f"失败 {code}: {d}"))
        return
    if raw:
        print(json.dumps(d, ensure_ascii=False, indent=2))
        return

    msgs = d.get("messages", [])
    print(f"model = {d.get('model')}   messages = {len(msgs)}   "
          f"tools = {[t['function']['name'] for t in d.get('tools', [])]}")
    print(c(DIM, f"stream_options = {json.dumps(d.get('stream_options'))}"))
    total = 0
    for i, m in enumerate(msgs):
        content = m.get("content") or ""
        if isinstance(content, list):  # 多模态
            content = json.dumps(content, ensure_ascii=False)
        size = len(content)
        total += size
        extra = ""
        if m.get("tool_calls"):
            extra = " tool_calls=" + ",".join(tc["function"]["name"] for tc in m["tool_calls"])
        head = content.split("\n")[0][:70].replace("\n", " ")
        print(f"  [{i:3}] {m.get('role'):9} {size:7} B  {c(DIM, head)}{c(WARN, extra)}")
    print(c(DIM, f"合计 {total} 字符（历史越靠前越是被缓存命中的部分）"))


def print_prompt() -> None:
    code, d = call("GET", "/api/debug/state")
    if code != 200:
        print(c(BAD, f"失败 {code}: {d}"))
        return
    sp = d.get("systemPrompt", {})
    if sp.get("overridden"):
        print(f"系统提示词：{sp.get('chars')} 字符（{c(WARN, '运行时覆盖中')}，重启失效）")
    else:
        print(f"系统提示词：{sp.get('chars')} 字符（编译进去的那版）")
    print(c(DIM, "看正文：core/src/agent.rs 的 SYSTEM_PROMPT，或 `cargo run -q --example dump_prompt`"))


def set_prompt(text: str | None) -> None:
    code, d = call("POST", "/api/debug/system-prompt", {"text": text or ""})
    if code != 200:
        print(c(BAD, f"失败 {code}: {d}"))
        return
    if d.get("overridden"):
        print(c(OK, f"已覆盖（{d['chars']} 字符）。老会话的前缀不会变，"
                    f"新提示词会作为一条 system 消息追加在历史末尾。"))
    else:
        print(c(OK, "已恢复成编译进去的那版。"))


def print_snapshot() -> None:
    # 让手机重读一次界面：走「人按按钮」那条路（/api/command），
    # 设备执行完会把新快照推回 /api/device/snapshot。
    call("POST", "/api/command", {"cmd": {"type": "snapshot"}})
    time.sleep(0.6)
    code, d = call("GET", "/api/snapshot")
    if code != 200 or not isinstance(d, dict):
        print(c(BAD, f"没有快照（{code}）：{d}"))
        return
    screen = d.get("screen") or {}
    print(f"包名 {d.get('package')}   Activity {d.get('activity')}   "
          f"屏幕 {screen.get('w')}x{screen.get('h')}   {len(d.get('nodes', []))} 个节点")
    for n in d.get("nodes", []):
        flags = n.get("a") or {}
        txt = (n.get("t") or "").replace("\n", " ")
        caps = [k for k in ("clickable", "long_clickable", "editable", "scrollable", "checkable")
                if flags.get(k)]
        if txt == "" and not caps:
            continue
        if not flags.get("enabled", True):
            caps.append("disabled")
        print(f"  [{n.get('i'):3}] {n.get('c'):18} {txt[:40]:42} {tuple(n.get('b') or [])} {caps}")


def cmd_bash(script: str) -> None:
    code, d = call("POST", "/api/linux", {"command": script}, timeout=900)
    if code != 200 or not isinstance(d, dict):
        print(c(BAD, f"失败 {code}: {d}"))
        return
    head = "exit=" + str(d.get("exit_code"))
    if d.get("truncated"):
        head += "  已截断（完整输出 " + str(d.get("full_output_path")) + "）"
    print(c(OK if d.get("ok") else BAD, head))
    out = str(d.get("output", ""))
    print(indent(out, 200) if out.strip() else c(DIM, "(无输出)"))


def cmd_cat(path: str) -> None:
    code, raw = call("GET", "/sandbox" + (path if path.startswith("/") else "/" + path))
    if code != 200:
        print(c(BAD, f"失败 {code}: {raw}"))
        return
    print(raw if isinstance(raw, str) else json.dumps(raw, ensure_ascii=False))


def cmd_dev(js: str) -> None:
    try:
        cmd = json.loads(js)
    except json.JSONDecodeError as e:
        print(c(BAD, f"不是合法 JSON：{e}"))
        return
    code, d = call("POST", "/api/command", {"cmd": cmd})
    print(json.dumps(d, ensure_ascii=False) if code == 200 else c(BAD, f"失败 {code}: {d}"))


def cmd_sessions() -> None:
    code, d = call("GET", "/api/sessions")
    if code != 200:
        print(c(BAD, f"失败 {code}: {d}"))
        return
    for s in d.get("sessions", []):
        mark = "*" if s.get("active") else " "
        print(f" {mark} {s['id']}  {s.get('count', 0):3} 条  {s.get('title')}")


def handle_command(line: str) -> bool:
    """返回 False 表示要退出。"""
    cmd, _, rest = line.partition(" ")
    rest = rest.strip()

    if cmd in ("/quit", "/exit", "/q"):
        return False
    elif cmd == "/help":
        print(HELP)
    elif cmd == "/state":
        print_state()
    elif cmd == "/request":
        print_request(raw=(rest == "raw"))
    elif cmd == "/prompt":
        if rest == "-":
            print(c(DIM, "（读到 EOF 为止，Ctrl-D 结束）"))
            set_prompt(sys.stdin.read())
        elif rest:
            set_prompt(rest)
        else:
            print_prompt()
    elif cmd == "/prompt!":
        set_prompt(None)
    elif cmd == "/bash":
        if rest:
            cmd_bash(rest)
        else:
            print(c(WARN, "用法：/bash <命令>"))
    elif cmd == "/cat":
        cmd_cat(rest)
    elif cmd == "/snap":
        print_snapshot()
    elif cmd == "/dev":
        cmd_dev(rest)
    elif cmd == "/sessions":
        cmd_sessions()
    elif cmd == "/new":
        code, d = call("POST", "/api/sessions")
        print(c(OK, f"新会话 {d.get('id')}") if code == 200 else c(BAD, f"失败 {code}: {d}"))
    elif cmd == "/use":
        code, d = call("POST", "/api/sessions/activate", {"id": rest})
        print(c(OK, f"切到 {rest}") if code == 200 else c(BAD, f"失败 {code}: {d}"))
    elif cmd == "/clear":
        code, _ = call("POST", "/api/reset")
        print(c(OK, "当前会话已清空") if code == 204 else c(BAD, f"失败 {code}"))
    elif cmd == "/wait":
        d = wait_until_idle()
        print_server_view(d)
        print(c(DIM, "服务端已空闲" if d and not d.get("busy") else "还在跑"))
    else:
        print(c(WARN, f"不认识的命令 {cmd}，/help 看列表"))
    return True


def repl() -> int:
    # 管道里也能用（每行当一条命令/一句话），所以提示符和横幅只在终端里打。
    tty = sys.stdin.isatty()
    code, d = call("GET", "/api/debug/state")
    if code != 200:
        print(c(BAD, f"连不上 {BASE}（{code}）{d}"))
        print(c(DIM, "手机上跑着？先 `just forward`。本机跑着？`just dev`。"))
        return 1
    if tty:
        where = "手机" if d.get("device") else ("本机 mock" if d.get("mock") else "本机（无设备）")
        print(c(DIM, f"连上 {BASE} · {where} · 会话 {d.get('sessions', {}).get('count')} 个"
                     f" · 模型 {(d.get('llm') or {}).get('model')}"))
        print(c(DIM, "/help 看命令，Ctrl-D 退出"))

    while True:
        try:
            line = input(c("1", "\n> ") if tty else "").strip()
        except (EOFError, KeyboardInterrupt):
            if tty:
                print()
            return 0
        if not line:
            continue
        if line.startswith("/"):
            if not handle_command(line):
                return 0
        else:
            do_turn(line, as_json=False)


# ---------------------------------------------------------------------------


def main() -> int:
    global BASE

    ap = argparse.ArgumentParser(
        description="从电脑上驱动 android-agent 的核心",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="例子：\n"
               "  just phone \"看看现在屏幕上有什么\"\n"
               "  python3 scripts/agentctl.py --json \"在沙箱里算 2**100\"\n"
               "  python3 scripts/agentctl.py            # 交互模式\n",
    )
    ap.add_argument("prompt", nargs="*", help="要发给 agent 的话；不给就进交互模式")
    ap.add_argument("--base", help=f"核心地址（默认 {BASE}，也可用 $AGENT_BASE）")
    ap.add_argument("--json", action="store_true", help="输出一整份机器可读转录")
    ap.add_argument("--timeout", type=float, default=1800.0, help="单个回合的总超时秒数")
    args = ap.parse_args()

    global TIMEOUT
    TIMEOUT = args.timeout

    if args.base:
        BASE = args.base.rstrip("/")

    if not args.prompt:
        # 没有提示词就进交互。从管道喂进来時每行当一条命令/一句话 ——
        # 这样 /state、/bash 这些也能被脚本驱动。
        return repl()

    return do_turn(" ".join(args.prompt), args.json)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except urllib.error.URLError as e:
        print(f"连不上 {BASE}：{e}", file=sys.stderr)
        print("手机上跑着？先 `just forward`。本机跑着？`just dev`。", file=sys.stderr)
        sys.exit(1)
