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
    /dev {"type":"key","name":"back"}    直接发设备指令
    /doctor       各部件体检（也可以直接 `just doctor`）
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


# 核心的 /api 要凭据（手机上别的 App 也能连 127.0.0.1）。`just phone` /
# `just doctor` 会用 scripts/phone-token.sh 拿到再放进环境变量。
TOKEN = os.environ.get("AGENT_TOKEN", "")


def headers() -> dict:
    h = {"content-type": "application/json"}
    if TOKEN:
        h["x-agent-token"] = TOKEN
    return h


def call(method: str, path: str, body=None, timeout: float = 60.0):
    """普通请求。返回 (状态码, 解析后的 body 或原始文本)。"""
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(
        BASE + path,
        data=data,
        method=method,
        headers=headers(),
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
        headers=headers(),
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


def fmt_bytes(n) -> str:
    """人类可读的字节数，跟核心 reader::fmt_size 一致。"""
    if not isinstance(n, (int, float)) or n < 0:
        return "?"
    for unit, div in (("GB", 1024**3), ("MB", 1024**2), ("KB", 1024)):
        if n >= div:
            return f"{n / div:.1f}{unit}"
    return f"{int(n)}B"


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
    if name == "read":
        if out.get("imagePath"):
            bits = [
                str(out.get("imageMime", "图片")),
                f"{out.get('imageWidth', '?')}×{out.get('imageHeight', '?')}",
            ]
            if isinstance(out.get("totalBytes"), (int, float)):
                bits.append(fmt_bytes(out["totalBytes"]))
        else:
            bits = [f"{out.get('totalLines', '?')} 行"]
            if isinstance(out.get("totalBytes"), (int, float)):
                bits.append(fmt_bytes(out["totalBytes"]))
            if out.get("truncated"):
                bits.append("已截断")
        head = "   " + c(OK, " ".join(bits))
        body = str(out.get("output", ""))
        return f"{head}\n{indent(body, 25)}" if body.strip() else head
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
# 写进哪个会话
# ---------------------------------------------------------------------------

# 调试专用会话的标题。**默认**用它，而不是用户当前打开的那条 ——
# 踩过：一句测试消息被追加进了用户正在聊的会话里。
SCRATCH = "调试（agentctl）"

# None = 用服务端的当前活动会话（前端的默认行为）
TARGET: str | None = None


def pick_session(which: str) -> None:
    """决定这一程写进哪个会话（直接设到模块级 `TARGET`）。

    **自己设全局、不返回** —— 免得调用方忘了写 `global TARGET`。
    这个坑真踩过：`main()` 里少了 `global`，`TARGET` 变成局部变量，
    模块级那份一直是 None，于是消息全都写进了**用户当前打开的**那条会话。
    """
    global TARGET
    if which == "active":
        print(c(WARN, "⚠ --session active：会写进你当前打开的那条会话"), file=sys.stderr)
        TARGET = None
        return
    if which not in ("", "scratch"):
        # 显式给了 id 就**不动**活动会话 —— run_turn 是按 id 找会话的，
        # 没必要顺手把用户正开着的那条切走。（要切用 /use）
        TARGET = which
        return

    # find-or-create：同一条调试会话反复用，历史能连着，也不会在侧边栏堆一堆
    code, d = call("GET", "/api/sessions")
    was_active = None
    if code == 200 and isinstance(d, dict):
        for sess in d.get("sessions") or []:
            if sess.get("title") == SCRATCH:
                TARGET = str(sess["id"])
                return
            if sess.get("active"):
                was_active = str(sess["id"])

    # 服务端的「新建会话」会顺手把它设成活动会话。第一次建调试会话时把用户
    # 原来那条**换回去** —— 打开 App 不该被我们从 CLI 里切走。（要切用 /use）
    code, d = call("POST", "/api/sessions")
    if code != 200 or not isinstance(d, dict):
        print(c(WARN, f"建不了调试会话（{code}），改写到当前活动会话"), file=sys.stderr)
        TARGET = None
        return
    sid = str(d.get("id"))
    call("POST", "/api/sessions/rename", {"id": sid, "title": SCRATCH})
    if was_active:
        call("POST", "/api/sessions/activate", {"id": was_active})
    print(c(DIM, f"建了一条调试会话「{SCRATCH}」—— 你的对话和当前打开的那条都不动"),
          file=sys.stderr)
    TARGET = sid


def session_note() -> str:
    if TARGET is None:
        return c(WARN, "写进当前活动会话")
    return c(DIM, f"会话 {TARGET}")


# ---------------------------------------------------------------------------
# 一个回合
# ---------------------------------------------------------------------------


def wait_until_idle(timeout: float = 1800.0) -> dict | None:
    """轮询这一程的会话（`TARGET`；None = 活动会话）直到它不在跑。SSE 断了的时候用它兜底。

    「进行中」是按会话记的（几个会话可以同时各跑一个），所以要问**自己那条**
    会话 —— 不带 id 问到的是用户当前打开的那条。
    """
    path = "/api/session" + (f"?id={TARGET}" if TARGET else "")
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        code, d = call("GET", path)
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
    body: dict = {"text": prompt}
    if TARGET:
        body["session"] = TARGET
    dropped = False
    try:
        for ch in sse("/api/chat", body, timeout=TIMEOUT):
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
  /doctor [deep]         各部件体检：核心/模型/沙箱/设备/会话库（deep 会真跑一轮对话）
  /state                 服务端完整状态（/api/debug/state）
  /request [raw]         下一次会发给模型的完整请求体
  /prompt                看当前系统提示词
  /sessions              列会话
  /models [refresh]      列 provider 的模型（👁 = 能看图；refresh 强制重拉目录）
  /model <id>            切到某个模型（自动重判能不能看图）

{c('1', '改')}
  /prompt <文字>         运行时换系统提示词 —— 不用重装 APK
  /prompt -              从 stdin 读到 EOF（heredoc 好用）
  /prompt!               恢复成编译进去的那版
  /new                   新建会话      /use <id>   切会话     /clear  清空当前会话
                         （默认写进「调试（agentctl）」那条会话，不动你正在聊的；
                           /use active 改成写进当前活动会话）

{c('1', '直连（绕过模型）')}
  /bash [--timeout 秒] <命令>   在沙箱里跑命令（/api/linux）
  /cat <沙箱路径>        读沙箱里的文件（/sandbox/…）
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


def cmd_bash(script: str) -> None:
    # 可选：/bash --timeout 3600 <命令>（长下载/编译用）。不传用服务端默认。
    body = {"command": script}
    client_timeout = 900.0
    if script.startswith("--timeout"):
        parts = script.split(None, 2)
        if len(parts) < 3 or not parts[1].isdigit():
            print(c(WARN, "用法：/bash --timeout <秒> <命令>"))
            return
        secs = int(parts[1])
        body = {"command": parts[2], "timeout_ms": secs * 1000}
        client_timeout = secs + 60
    code, d = call("POST", "/api/linux", body, timeout=client_timeout)
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
    # 沙箱文件的 URL 带一把钥匙（/sandbox/<key>/…），从 health 拿
    _, h = call("GET", "/api/health")
    key = h.get("sandboxKey", "") if isinstance(h, dict) else ""
    code, raw = call("GET", f"/sandbox/{key}" + (path if path.startswith("/") else "/" + path))
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


def cmd_models(refresh: bool = False) -> None:
    code, d = call("GET", "/api/models" + ("?refresh=1" if refresh else ""))
    if code != 200:
        print(c(BAD, f"失败 {code}: {d}"))
        return
    print(f"{d.get('name')} · {d.get('baseUrl')} · 当前 {d.get('current')}（能看图：{d.get('vision')}）")
    for m in d.get("models", []):
        mark = "*" if m.get("id") == d.get("current") else " "
        eye = "👁" if m.get("vision") else "  "
        ctx = m.get("context_window")
        window = f"{(ctx // 1000)}K" if ctx else ""
        print(f" {mark} {eye} {m.get('id'):<34} {window}")


def cmd_model(model: str) -> None:
    code, d = call("POST", "/api/model", {"model": model})
    if code != 200:
        print(c(BAD, f"失败 {code}: {d}"))
        return
    print(c(OK, f"已切到 {d.get('model')}（能看图：{d.get('vision')}）"))


def cmd_doctor(deep: bool = False) -> int:
    """把「这台设备上各个部件还活着吗」过一遍。

    只做**没有副作用**的检查：不按手机按键、不改文件、不装包。所以随时能跑，
    也可以跑到一半 Ctrl-C。
    只依赖 `/api/health`（连旧构建也有）+ 一条 `echo ok`，其余是加分项。
    """
    bad = 0

    def row(name: str, good: bool, detail: str = "") -> None:
        nonlocal bad
        if not good:
            bad += 1
        print(f"  {c(OK, '✅') if good else c(BAD, '❌')} {name:<8} {detail}")

    code, h = call("GET", "/api/health")
    if code != 200 or not isinstance(h, dict):
        row("核心", False, f"{BASE} 连不上（{code}）{str(h)[:120]}")
        print(c(DIM, "  手机上跑着吗？先 `just wifi`；本机就 `just dev`"))
        return 1
    tag = "（mock 设备）" if h.get("mock") else ""
    row("核心", True, f"v{h.get('version')} · {h.get('platform')} · 端口 {h.get('port')} {tag}")

    llm = h.get("llm") or {}
    row("模型", bool(llm), (f"{llm.get('name')} / {llm.get('model')}" if llm
                            else "没配 —— 前端「模型设置」或 POST /api/llm"))

    dev = h.get("device")
    row("设备", bool(dev), str(dev) if dev else "没有设备实现（真机上＝无障碍服务没连）")

    linux = h.get("linux") or {}
    rootfs = linux.get("rootfs")
    if linux.get("host"):
        row("沙箱", True, f"宿主模式（无沙箱）：bash 直接跑在 {rootfs or '/'} 这台机器上")
    else:
        row("沙箱", bool(rootfs), str(rootfs) if rootfs else "没配（真机上要等 rootfs 解压完）")

    # 浏览器后端：手机上=壳里的 WebView；桌面上=CDP + 调试端口
    browser = h.get("browser")
    row("浏览器", bool(browser),
        str(browser) if browser else "没接（桌面上是 AGENT_CDP=off，或者自己起的 Chrome 起不来）")

    # 真跑一句。只读、无副作用 —— 但它是唯一能证明命令通道**真的能跑**的检查
    t0 = time.time()
    code, d = call("POST", "/api/linux", {"command": "echo ok"}, timeout=180)
    dt = (time.time() - t0) * 1000
    good = code == 200 and isinstance(d, dict) and bool(d.get("ok"))
    row("跑命令", good,
        f"{dt:.0f}ms · {str(d.get('output', '')).strip()!r}" if good else f"{code} {str(d)[:120]}")

    code, ss = call("GET", "/api/sessions")
    n = len(ss.get("sessions", [])) if code == 200 and isinstance(ss, dict) else None
    row("会话库", n is not None, f"{n} 个会话" if n is not None else f"{code} {str(ss)[:80]}")

    code, st = call("GET", "/api/debug/state")
    if code == 200 and isinstance(st, dict):
        row("调试接口", True, f"可用 · 提示词 {st.get('systemPrompt', {}).get('chars')} 字符"
            + ("（运行时覆盖过）" if st.get("systemPrompt", {}).get("overridden") else ""))
        if st.get("dataDir"):
            print(c(DIM, f"           dataDir {st['dataDir']}"))
        if st.get("busySessions"):
            print(c(WARN, f"           ⚠ 服务端此刻有 {len(st['busySessions'])} 个会话在跑回合（/wait 等它）"))
    else:
        print(c(DIM, "  · 这台核心没有 /api/debug/* —— 装的是旧构建"))

    if deep:
        print(c(DIM, "  ── 真跑一轮对话（会花 token）──"))
        tr = Transcript(live=False)
        t0 = time.time()
        try:
            for ch in sse("/api/chat", {"text": "只回两个字：好了"}, timeout=300):
                tr.feed(ch)
        except Exception as e:  # noqa: BLE001
            tr.errors.append(str(e))
        dt = time.time() - t0
        tools = [t["name"] for t in tr.tools.values()]
        row("一轮对话", not tr.errors,
            f"{dt:.1f}s · 工具 {tools or '无'} · {tr.summary() or '无用量'}")

    print()
    print(c(OK, "全部正常") if not bad else c(BAD, f"{bad} 项有问题"))
    return 1 if bad else 0


def handle_command(line: str) -> bool:
    """返回 False 表示要退出。"""
    cmd, _, rest = line.partition(" ")
    rest = rest.strip()

    if cmd in ("/quit", "/exit", "/q"):
        return False
    elif cmd == "/help":
        print(HELP)
    elif cmd == "/doctor":
        cmd_doctor(deep=(rest == "deep"))
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
    elif cmd == "/dev":
        cmd_dev(rest)
    elif cmd == "/sessions":
        cmd_sessions()
    elif cmd == "/models":
        cmd_models(refresh=(rest == "refresh"))
    elif cmd == "/model":
        if rest:
            cmd_model(rest)
        else:
            print(c(WARN, "用法：/model <id>（/models 看列表）"))
    elif cmd == "/new":
        code, d = call("POST", "/api/sessions")
        print(c(OK, f"新会话 {d.get('id')}") if code == 200 else c(BAD, f"失败 {code}: {d}"))
    elif cmd == "/use":
        global TARGET
        if rest == "active":
            TARGET = None
            print(c(WARN, "改成写进当前活动会话"))
        else:
            code, d = call("POST", "/api/sessions/activate", {"id": rest})
            if code == 200:
                TARGET = rest
                print(c(OK, f"切到 {rest}"))
            else:
                print(c(BAD, f"失败 {code}: {d}"))
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
        print(c(DIM, f"{session_note()} · /help 看命令，Ctrl-D 退出"))

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
    ap.add_argument("--token", help="核心的访问凭据（默认 $AGENT_TOKEN；手机上用 scripts/phone-token.sh 拿）")
    ap.add_argument("--json", action="store_true", help="输出一整份机器可读转录")
    ap.add_argument("--timeout", type=float, default=1800.0, help="单个回合的总超时秒数")
    ap.add_argument("--doctor", action="store_true", help="各部件体检一遍就退出")
    ap.add_argument("--session", default="scratch", metavar="WHICH",
                    help="写进哪个会话：scratch（默认，专用调试会话）/ active（当前活动会话）/ <会话 id>")
    ap.add_argument("--deep", action="store_true", help="体检时另外真跑一轮对话（花 token）")
    args = ap.parse_args()

    global TIMEOUT
    TIMEOUT = args.timeout

    if args.base:
        BASE = args.base.rstrip("/")
    if args.token:
        global TOKEN
        TOKEN = args.token

    # 定好这一程写进哪个会话。默认是一条**专用调试会话** —— 直接在 CLI 里
    # 发消息而不小心写进用户正在聊的那条，是真踩过的坑。
    pick_session(args.session)

    if args.doctor:
        return cmd_doctor(args.deep)

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
