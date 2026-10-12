#!/usr/bin/env python3
"""端到端冒烟测试：真实 LLM + 工具执行。

前提：core 已在 8787 跑着，且配好了 provider 环境变量。

    just dev                 # 另一个终端
    python3 scripts/smoke.py

它会走完整条链路：

    用户请求 → 模型调 bash（跑命令）→ 结果回灌 → 模型给出最终答复

工具一律直接执行，没有审批环节。
"""

import json
import sys
import urllib.request

import os
BASE = os.environ.get("AGENT_BASE", "http://127.0.0.1:8787")
# 核心设了凭据时要带（桌面默认不设；对着手机跑见 scripts/phone-token.sh）
HEADERS = {"content-type": "application/json"}
if os.environ.get("AGENT_TOKEN"):
    HEADERS["x-agent-token"] = os.environ["AGENT_TOKEN"]


def stream(path: str, body: dict) -> dict:
    """读 SSE，把 chunk 折叠成可读转录。"""
    req = urllib.request.Request(
        BASE + path,
        data=json.dumps(body).encode(),
        headers=HEADERS,
    )
    text, reasoning, tools, errs = "", "", {}, []
    outputs: dict[str, object] = {}
    with urllib.request.urlopen(req, timeout=180) as r:
        buf = b""
        while True:
            b = r.read1(4096)
            if not b:
                break
            buf += b
            while b"\n\n" in buf:
                frame, buf = buf.split(b"\n\n", 1)
                for line in frame.decode("utf-8", "replace").split("\n"):
                    if not line.startswith("data:"):
                        continue
                    payload = line[5:].strip()
                    if payload == "[DONE]":
                        return {
                            "text": text,
                            "reasoning": reasoning,
                            "tools": tools,
                            "outputs": outputs,
                            "errs": errs,
                        }
                    c = json.loads(payload)
                    t = c["type"]
                    if t == "text-delta":
                        text += c["delta"]
                    elif t == "reasoning-delta":
                        reasoning += c["delta"]
                    elif t == "tool-input-available":
                        tools[c["toolCallId"]] = [c["toolName"], c["input"], None]
                    elif t == "tool-output-available":
                        outputs[c["toolCallId"]] = c["output"]
                        if c["toolCallId"] in tools:
                            tools[c["toolCallId"]][2] = c["output"]
                    elif t == "error":
                        errs.append(c["errorText"])
    return {
        "text": text,
        "reasoning": reasoning,
        "tools": tools,
        "outputs": outputs,
        "errs": errs,
    }


def show(tag: str, r: dict) -> dict:
    print(f"\n{'=' * 68}\n{tag}\n{'=' * 68}")
    if r["reasoning"]:
        print(f"[思考] {r['reasoning'][:220]}")
    if r["text"]:
        print(f"[回答] {r['text'][:500]}")
    for _cid, (name, inp, out) in r["tools"].items():
        state = "已执行" if out is not None else "未执行"
        print(f"[工具] {name}({json.dumps(inp, ensure_ascii=False)}) -> {state}")
        if out:
            print(f"       {str(out)[:200]}")
    for e in r["errs"]:
        print(f"[错误] {e}")
    return r


def main() -> int:
    try:
        r = show(
            "第 1 轮：用户请求",
            stream(
                "/api/chat",
                {
                    "text": "在 /workspace 建一个 hello.sh，内容是 echo hello，"
                    "运行它，然后把它改成输出 hello world 再跑一次。"
                },
            ),
        )
    except Exception as e:  # noqa: BLE001
        print(f"请求失败：{e}\n（core 起来了吗？provider 配了吗？）")
        return 1

    if r["errs"]:
        print(f"\n❌ 回合里有错误：{r['errs']}")
        return 1

    ran = sorted({name for name, _inp, out in r["tools"].values() if out is not None})
    if "bash" not in ran:
        print(f"\n❌ 没有看到 bash 成功执行（执行过的工具：{ran}）")
        return 1
    if not r["text"].strip():
        print("\n❌ 模型没有给出最终答复")
        return 1

    print(f"\n✅ 全链路通过：用户请求 → 工具执行（{', '.join(ran)}）→ 结果回灌 → 模型复述")
    return 0


if __name__ == "__main__":
    sys.exit(main())
