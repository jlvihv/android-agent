#!/usr/bin/env python3
"""核心的压测脚本（本地假 LLM，不花 token、不联网）。

## 能压什么、压的是哪条路径

它不是「把 HTTP 打满」那种压测 —— 核心是个**单用户 agent 后端**，真正会
出问题的地方不是 QPS，而是：

    S1 并发对话   几十路 SSE 同时开着，各自跑 agent loop、各自落盘、共用
                  一个 SQLite、一个会话锁、一个有界阻塞池（4 线程）。
    S2 慢客户端   客户端开了连接却**不读**（WebView 被挂起、手机切走）。
                  核心会不会无界堆积？断开之后资源放不放得掉？
    S3 突发连接   一次 200 条 TCP 打进来（局域网里一堆设备同时连）。
    S4 静默流     上游 200 但一个事件都不发 —— 核心不能永远挂着（这是
                  「回合卡死」的经典成因）。**实测结论**：核心会把它当网络
                  错误、按 2s/5s/10s 退避重试 3 次（约 65 秒）后报错结束 ——
                  不会卡死，这点是对的。 `DATA_IDLE`（300s）那条路没走到，
                  因为传输层先断了。
    S5 失败重试   上游按概率回 429 —— 重试退避会不会把连接池/线程占死。
    S6 长会话     一个会话里连着发 N 条，历史越滚越长（每步都重新组装请求）。

## 用法

    # 1) 起一套隔离的核心 + 假 LLM（`--auto` 会自动起；也可以自己起好再跑）
    python3 scripts/stress.py --auto --scenario all

    # 2) 只跑某一个，把并发调大
    python3 scripts/stress.py --base http://127.0.0.1:18899 --token … \\
        --scenario s1 --concurrency 50 --rounds 5

**不会碰你正在用的那个核心**：`--auto` 用的是独立端口 + 独立
`XDG_DATA_HOME`（`AGENT_MULTI=1` 绕开单实例锁，见 `core/src/single.rs`）。
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CORE_BIN = os.path.join(ROOT, "core", "target", "release", "agent-dev")


# ---------------------------------------------------------------------------
# SSE 客户端
# ---------------------------------------------------------------------------


@dataclass
class StreamResult:
    ok: bool = False
    chunks: int = 0
    text: int = 0
    first_chunk_ms: float = 0.0
    total_ms: float = 0.0
    error: str = ""
    cancelled: bool = False
    events: list[str] = field(default_factory=list)


def open_stream(
    base: str,
    path: str,
    body: dict | None,
    token: str | None,
    timeout: float,
    *,
    read_delay: float = 0.0,
    stop_after: int | None = None,
) -> StreamResult:
    """开一条 SSE 并读到 [DONE]（或 stop_after 条事件后主动断开）。

    `read_delay` 让客户端每收一块就睡一会儿 —— 用来模拟「读得慢的客户端」，
    这是压测 S2 的核心手段：服务端会因此堆积，暴露无界通道 / OOM 问题。
    """
    r = StreamResult()
    t0 = time.monotonic()
    headers = {"content-type": "application/json"}
    if token:
        headers["x-agent-token"] = token
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, headers=headers)
    buf = b""
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            while True:
                piece = resp.read1(4096)
                if not piece:
                    break
                if r.first_chunk_ms == 0.0:
                    r.first_chunk_ms = (time.monotonic() - t0) * 1000
                buf += piece
                while b"\n\n" in buf:
                    frame, buf = buf.split(b"\n\n", 1)
                    for line in frame.decode("utf-8", "replace").split("\n"):
                        if not line.startswith("data:"):
                            continue
                        payload = line[5:].strip()
                        if payload == "[DONE]":
                            r.ok = True
                            r.total_ms = (time.monotonic() - t0) * 1000
                            return r
                        try:
                            c = json.loads(payload)
                        except json.JSONDecodeError:
                            continue
                        r.chunks += 1
                        t = c.get("type", "")
                        r.events.append(t)
                        if t == "text-delta":
                            r.text += len(c.get("delta", ""))
                        if t == "error":
                            r.error = c.get("errorText", "unknown")
                        if stop_after is not None and r.chunks >= stop_after:
                            # 客户端主动断开：这是场景本身，不是失败
                            r.ok = True
                            r.cancelled = True
                            r.total_ms = (time.monotonic() - t0) * 1000
                            return r
                if read_delay > 0:
                    time.sleep(read_delay)
        # 连接关了但没见 [DONE]
        r.total_ms = (time.monotonic() - t0) * 1000
        r.error = r.error or "流在 [DONE] 之前结束了"
    except urllib.error.HTTPError as e:
        r.total_ms = (time.monotonic() - t0) * 1000
        r.error = f"HTTP {e.code}"
    except Exception as e:  # 超时、连接重置
        r.total_ms = (time.monotonic() - t0) * 1000
        r.error = f"{type(e).__name__}: {e}"
    return r


def api(base: str, path: str, body: dict | None, token: str | None, timeout: float = 30):
    headers = {"content-type": "application/json"}
    if token:
        headers["x-agent-token"] = token
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, headers=headers)
    with urllib.request.urlopen(req, timeout=timeout) as r:
        raw = r.read()
    try:
        return json.loads(raw) if raw else None
    except json.JSONDecodeError:
        return None  # 首页是 HTML，就绪探测用得上


# ---------------------------------------------------------------------------
# 统计
# ---------------------------------------------------------------------------


def pct(vals: list[float], p: float) -> float:
    if not vals:
        return 0.0
    s = sorted(vals)
    i = min(len(s) - 1, int(round((p / 100) * (len(s) - 1))))
    return s[i]


@dataclass
class Summary:
    name: str = ""
    ok: int = 0
    cancelled: int = 0
    failed: int = 0
    errors: dict[str, int] = field(default_factory=dict)
    total: list[float] = field(default_factory=list)
    first: list[float] = field(default_factory=list)
    text_chars: list[int] = field(default_factory=list)

    def add(self, r: StreamResult) -> None:
        self.total.append(r.total_ms)
        if r.first_chunk_ms:
            self.first.append(r.first_chunk_ms)
        self.text_chars.append(r.text)
        if r.cancelled:
            self.cancelled += 1
            self.ok += 1
        elif r.ok:
            self.ok += 1
        else:
            self.failed += 1
            key = r.error[:60] or "unknown"
            self.errors[key] = self.errors.get(key, 0) + 1

    def merge(self, o: "Summary") -> None:
        self.ok += o.ok
        self.cancelled += o.cancelled
        self.failed += o.failed
        for k, v in o.errors.items():
            self.errors[k] = self.errors.get(k, 0) + v
        self.total += o.total
        self.first += o.first
        self.text_chars += o.text_chars

    def report(self) -> str:
        n = self.ok + self.failed
        if not n:
            return f"{self.name}: 没有样本"
        lines = [
            f"{self.name}: {n} 条  ok={self.ok}"
            + (f"（其中 {self.cancelled} 条客户端主动断开）" if self.cancelled else "")
            + f"  fail={self.failed}",
            f"  端到端  p50={pct(self.total,50):.0f}ms  p95={pct(self.total,95):.0f}ms  "
            f"max={max(self.total):.0f}ms",
        ]
        if self.first:
            lines.append(
                f"  首字节  p50={pct(self.first,50):.0f}ms  p95={pct(self.first,95):.0f}ms"
            )
        if self.text_chars and max(self.text_chars):
            lines.append(f"  正文字数 中位 {int(statistics.median(self.text_chars))}")
        for k, v in sorted(self.errors.items(), key=lambda kv: -kv[1]):
            lines.append(f"  ! {v}× {k}")
        return "\n".join(lines)


def run_parallel(fn, n: int, stagger_ms: float = 0.0) -> Summary:
    """并发跑 n 个 fn()，把结果汇成一个 Summary。"""
    out = Summary()
    lock = threading.Lock()
    threads = []

    def wrap(i: int):
        if stagger_ms:
            time.sleep(i * stagger_ms / 1000)
        try:
            r = fn(i)
        except Exception as e:  # 线程里的异常不能吞
            r = StreamResult(error=f"线程异常 {type(e).__name__}: {e}")
        with lock:
            out.add(r)

    for i in range(n):
        t = threading.Thread(target=wrap, args=(i,), daemon=True)
        threads.append(t)
        t.start()
    for t in threads:
        t.join()
    return out


# ---------------------------------------------------------------------------
# 场景
# ---------------------------------------------------------------------------


def new_session(base: str, token: str | None) -> str:
    return api(base, "/api/sessions", {}, token)["id"]


def scenario_s1(base: str, token: str | None, n: int, rounds: int, timeout: float) -> Summary:
    """S1：n 路并发对话，每路 rounds 轮。"""
    total = Summary("S1 并发对话")
    sessions = [new_session(base, token) for _ in range(n)]

    def one(i: int) -> StreamResult:
        agg = StreamResult(ok=True)
        for r in range(rounds):
            res = open_stream(
                base,
                "/api/chat",
                {"text": f"压测 s1 第 {r} 轮 #{i}", "session": sessions[i]},
                token,
                timeout,
            )
            agg.chunks += res.chunks
            agg.text += res.text
            agg.total_ms += res.total_ms
            if not res.ok:
                agg.ok = False
                agg.error = res.error
                break
        return agg

    total.merge(run_parallel(lambda i: one(i), n, stagger_ms=20))
    return total


def scenario_s2(
    base: str, token: str | None, n: int, rounds: int, timeout: float, read_delay: float
) -> Summary:
    """S2：慢客户端 —— 开了流但不急着读，中途还可能直接断开。"""
    total = Summary(f"S2 慢客户端（每块后等 {read_delay:.2f}s，一半中途断开）")
    sessions = [new_session(base, token) for _ in range(n)]

    def one(i: int) -> StreamResult:
        # 一半的客户端收到 3 条事件就断开（模拟切走 App / 关页面）
        stop = 3 if i % 2 else None
        return open_stream(
            base,
            "/api/chat",
            {"text": f"压测 s2 #{i}", "session": sessions[i]},
            token,
            timeout,
            read_delay=read_delay,
            stop_after=stop,
        )

    # 中途断开的那些「没见到 [DONE]」是预期的，不计失败
    s = run_parallel(one, n, stagger_ms=10)
    total.merge(s)
    return total


def scenario_s3(base: str, token: str | None, n: int) -> Summary:
    """S3：一次打 n 条裸 TCP 连接（只看能不能都被接住/关掉，不发协议）。"""
    total = Summary(f"S3 突发 {n} 条连接")
    host, port = base.replace("http://", "").split(":")
    # 先建一条再关掉：区分「第一条连接慢」和「每条都慢」
    warm = time.monotonic()
    socket.create_connection((host, int(port)), timeout=10).close()
    print(f"  第一条连接 {(time.monotonic()-warm)*1000:.0f}ms")
    t0 = time.monotonic()
    socks = []
    refused = 0
    marks = []
    for i in range(n):
        try:
            s = socket.create_connection((host, int(port)), timeout=5)
            socks.append(s)
        except OSError:
            refused += 1
        if (i + 1) % 50 == 0:
            marks.append(((i + 1), (time.monotonic() - t0) * 1000))
    conn_ms = (time.monotonic() - t0) * 1000
    # 每 50 条一个时间点：卡在某个数上就是那一处排队了（backlog 满了 =
    # 卡在某个整数上跳 1 秒；均匀增长 = 正常）
    print("  进度：" + "  ".join(f"{k}条@{v:.0f}ms" for k, v in marks))
    for s in socks:
        s.close()
    total.ok = len(socks)
    total.failed = refused
    total.total.append(conn_ms)
    if refused:
        total.errors["连接被拒"] = refused
    print(f"  {n} 条连接用了 {conn_ms:.0f}ms，被拒 {refused}")
    return total


def scenario_s4(
    base: str, token: str | None, timeout: float, harness: "Harness | None" = None
) -> Summary:
    """S4：上游 200 但一个事件都不发 —— 核心必须自己判超时，不能永远挂着。

    **必须让假 LLM 处于 silent 模式**，否则它照常吐字，这条就测了个寂寞
    （踩过：`--scenario all` 里没换模式，S4「325ms 就成功」）。有 harness 时
    把假 LLM 重启成 silent 的，跑完再换回来。
    """
    total = Summary("S4 静默流（上游 200 + 不发事件）")
    if harness is not None:
        harness.restart_llm({**harness.llm_env, "LLM_SILENT": "1"})
    sid = new_session(base, token)
    r = open_stream(
        base, "/api/chat", {"text": "压测 s4 静默流", "session": sid}, token, timeout
    )
    total.add(r)
    print(f"  结果：{'按时报错' if not r.ok else '居然成功了'}  {r.total_ms:.0f}ms  {r.error}")
    return total


def scenario_s5(base: str, token: str | None, n: int, timeout: float) -> Summary:
    """S5：上游按概率 429 —— 看重试会不会把东西占死。"""
    total = Summary("S5 上游 429 重试")
    sessions = [new_session(base, token) for _ in range(n)]

    def one(i: int) -> StreamResult:
        return open_stream(
            base,
            "/api/chat",
            {"text": f"压测 s5 #{i}", "session": sessions[i]},
            token,
            timeout,
        )

    total.merge(run_parallel(one, n, stagger_ms=30))
    return total


def scenario_s6(base: str, token: str | None, n: int, timeout: float) -> Summary:
    """S6：一个会话连发 n 条 —— 历史越滚越长，每步都重新组装整个请求。"""
    total = Summary(f"S6 长会话（一个会话连发 {n} 条）")
    sid = new_session(base, token)
    marks = []
    for i in range(n):
        r = open_stream(
            base, "/api/chat", {"text": f"压测 s6 第 {i} 条：请回一段话", "session": sid}, token, timeout
        )
        total.add(r)
        marks.append((i, r.total_ms, len(r.events)))
        if not r.ok:
            print(f"  第 {i} 条就失败了，停：{r.error}")
            break
    # 时间随轮次的增长趋势（线性 = 正常；明显加速变差 = 请求体/历史组装有平方项）
    if len(marks) >= 4:
        first = statistics.mean(m[1] for m in marks[:2])
        last = statistics.mean(m[1] for m in marks[-2:])
        print(f"  前两条均值 {first:.0f}ms → 后两条均值 {last:.0f}ms（{last/max(first,1):.2f}×）")
    return total


# ---------------------------------------------------------------------------
# 起停隔离环境
# ---------------------------------------------------------------------------


class Harness:
    def __init__(self, port: int, llm_port: int, llm_env: dict[str, str]):
        self.port = port
        self.llm_port = llm_port
        self.llm_env = llm_env
        self.data = f"/tmp/agent-stress-{os.getpid()}"
        self.procs: list[subprocess.Popen] = []
        self.token: str | None = None

    def start(self) -> None:
        os.makedirs(self.data, exist_ok=True)
        llm = subprocess.Popen(
            ["bun", os.path.join(ROOT, "scripts", "fixture-llm-slow.ts")],
            env={**os.environ, "FIXTURE_PORT": str(self.llm_port), **self.llm_env},
            stdout=open("/tmp/stress-llm.log", "w"),
            stderr=subprocess.STDOUT,
        )
        self.procs.append(llm)
        time.sleep(1.5)

        core = subprocess.Popen(
            [CORE_BIN],
            env={
                **os.environ,
                "XDG_DATA_HOME": self.data,
                "AGENT_MULTI": "1",
                "AGENT_BIND": "127.0.0.1",
                "AGENT_PORT": str(self.port),
                "AGENT_LOG": "warn",
            },
            stdout=open("/tmp/stress-core.log", "w"),
            stderr=subprocess.STDOUT,
        )
        self.procs.append(core)
        base = f"http://127.0.0.1:{self.port}"
        for _ in range(50):
            time.sleep(0.2)
            try:
                api(base, "/", None, None, timeout=1)
                break
            except Exception:
                continue
        else:
            raise RuntimeError("核心没起来，看 /tmp/stress-core.log")
        # token 在启动输出里（AGENT_BIND=127.0.0.1 时其实 loopback 免登录，
        # 但统一带上更稳）
        with open("/tmp/stress-core.log") as f:
            for line in f:
                if "/auth?t=" in line:
                    self.token = line.split("/auth?t=")[1].strip()
                    break
        # 把假 LLM 配成 provider
        api(
            base,
            "/api/llm",
            {
                "name": "stress",
                "baseUrl": f"http://127.0.0.1:{self.llm_port}/v1",
                "model": "fixture-slow",
                "apiKey": "sk-stress",
                "apiStyle": "chat",
            },
            self.token,
        )
        print(f"环境就绪：{base}（假 LLM :{self.llm_port}），数据 {self.data}")

    def restart_llm(self, env: dict[str, str]) -> None:
        """换假 LLM 的行为（杀掉重起，端口不变）。"""
        self.llm_env = dict(env)
        old = self.procs.pop(0)
        old.send_signal(signal.SIGTERM)
        try:
            old.wait(timeout=5)
        except Exception:
            old.kill()
        llm = subprocess.Popen(
            ["bun", os.path.join(ROOT, "scripts", "fixture-llm-slow.ts")],
            env={**os.environ, "FIXTURE_PORT": str(self.llm_port), **self.llm_env},
            stdout=open("/tmp/stress-llm.log", "a"),
            stderr=subprocess.STDOUT,
        )
        self.procs.insert(0, llm)
        # 等端口重新可用
        for _ in range(50):
            time.sleep(0.1)
            try:
                socket.create_connection(("127.0.0.1", self.llm_port), timeout=1).close()
                return
            except OSError:
                continue
        raise RuntimeError("假 LLM 重启后没起来")

    def stop(self) -> None:
        for p in self.procs:
            try:
                p.send_signal(signal.SIGTERM)
            except Exception:
                pass
        for p in self.procs:
            try:
                p.wait(timeout=5)
            except Exception:
                p.kill()
        shutil.rmtree(self.data, ignore_errors=True)


def main() -> int:
    ap = argparse.ArgumentParser(description="android-agent 核心压测")
    ap.add_argument("--base", help="已有核心的地址；不给就自己起一套（--auto）")
    ap.add_argument("--token", default=os.environ.get("AGENT_TOKEN"))
    ap.add_argument("--auto", action="store_true", help="起一套隔离的核心 + 假 LLM")
    ap.add_argument("--port", type=int, default=18899)
    ap.add_argument("--llm-port", type=int, default=18791)
    ap.add_argument(
        "--scenario",
        default="all",
        help="all 或逗号分隔的 s1,s2,s3,s4,s5,s6",
    )
    ap.add_argument("--concurrency", type=int, default=8)
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--timeout", type=float, default=60)
    ap.add_argument(
        "--llm-env",
        default="",
        help="传给假 LLM 的环境变量，逗号分隔（如 LLM_GAP_MS=20,LLM_TOOL=1）",
    )
    args = ap.parse_args()

    llm_env = dict(
        kv.split("=", 1) for kv in args.llm_env.split(",") if "=" in kv
    )
    harness = None
    if args.auto or not args.base:
        harness = Harness(args.port, args.llm_port, llm_env)
        try:
            harness.start()
        except Exception as e:
            print(f"起环境失败：{e}")
            harness.stop()
            return 1
        base, token = f"http://127.0.0.1:{args.port}", harness.token
    else:
        base, token = args.base, args.token

    want = (
        {"s1", "s2", "s3", "s4", "s5", "s6"}
        if args.scenario == "all"
        else set(args.scenario.split(","))
    )
    results: list[Summary] = []
    try:
        if "s1" in want:
            print(f"\n=== S1 并发对话：{args.concurrency} 路 × {args.rounds} 轮 ===")
            results.append(scenario_s1(base, token, args.concurrency, args.rounds, args.timeout))
        if "s2" in want:
            print(f"\n=== S2 慢客户端：{args.concurrency} 路 ===")
            results.append(
                scenario_s2(base, token, args.concurrency, args.rounds, args.timeout, 0.05)
            )
        if "s3" in want:
            print("\n=== S3 突发连接 ===")
            results.append(scenario_s3(base, token, 200))
        if "s4" in want:
            print("\n=== S4 静默流 ===")
            results.append(scenario_s4(base, token, min(args.timeout, 30), harness))
        if "s6" in want:
            print(f"\n=== S6 长会话：连发 {args.rounds * 4} 条 ===")
            results.append(scenario_s6(base, token, args.rounds * 4, args.timeout))
    finally:
        print("\n" + "=" * 60)
        for s in results:
            print(s.report())
            print()
        if harness:
            # 收尾：看看核心还在不在、内存多少
            try:
                with open("/proc/%d/status" % harness.procs[1].pid) as f:
                    for line in f:
                        if line.startswith(("VmRSS", "Threads")):
                            print("  核心 " + line.strip())
            except Exception:
                pass
            harness.stop()
    return 0


if __name__ == "__main__":
    sys.exit(main())
