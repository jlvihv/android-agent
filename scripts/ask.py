#!/usr/bin/env python3
"""通用驱动器：给 agent 一句话，自动批准所有需要审批的工具调用。

    python3 scripts/ask.py "在 Linux 沙箱里看看内核信息，再算一下 2 的 100 次方"

core 需要已经在 8787 跑着。每轮都打印完整转录。
"""

import json
import sys
import urllib.request

import os
BASE = os.environ.get("AGENT_BASE", "http://127.0.0.1:8787")


def stream(path: str, body: dict) -> dict:
    req = urllib.request.Request(
        BASE + path,
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json"},
    )
    text, reasoning, tools, outputs, approvals, errs = "", "", {}, {}, [], []
    with urllib.request.urlopen(req, timeout=300) as r:
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
                    p = line[5:].strip()
                    if p == "[DONE]":
                        return {
                            "text": text,
                            "reasoning": reasoning,
                            "tools": tools,
                            "outputs": outputs,
                            "approvals": approvals,
                            "errs": errs,
                        }
                    c = json.loads(p)
                    t = c["type"]
                    if t == "text-delta":
                        text += c["delta"]
                    elif t == "reasoning-delta":
                        reasoning += c["delta"]
                    elif t == "tool-input-available":
                        tools[c["toolCallId"]] = (c["toolName"], c["input"])
                    elif t == "tool-output-available":
                        outputs[c["toolCallId"]] = c["output"]
                    elif t == "tool-approval-request":
                        approvals.append((c["approvalId"], c["toolCallId"], c["reason"]))
                    elif t == "error":
                        errs.append(c["errorText"])
    return {
        "text": text,
        "reasoning": reasoning,
        "tools": tools,
        "outputs": outputs,
        "approvals": approvals,
        "errs": errs,
    }


def show(round_no: int, r: dict) -> None:
    print(f"\n{'─' * 70}\n第 {round_no} 轮\n{'─' * 70}")
    if r["reasoning"].strip():
        print(f"💭 {r['reasoning'].strip()[:300]}")
    if r["text"].strip():
        print(f"🤖 {r['text'].strip()}")
    for cid, (name, inp) in r["tools"].items():
        out = r["outputs"].get(cid)
        arg = json.dumps(inp, ensure_ascii=False)
        if out is None:
            print(f"🔧 {name}({arg})  ← 未执行（等审批）")
        else:
            body = json.dumps(out, ensure_ascii=False)
            if isinstance(out, dict) and "stdout" in out:
                body = f"exit={out.get('exit')} " + repr(out.get("stdout", ""))[:600]
            print(f"🔧 {name}({arg})\n   → {body[:800]}")
    for e in r["errs"]:
        print(f"❌ {e}")


def main() -> int:
    prompt = " ".join(sys.argv[1:]) or "在 Linux 沙箱里看看内核信息和发行版，再算一下 2 的 100 次方"

    try:
        r = stream("/api/chat", {"text": prompt})
    except Exception as e:  # noqa: BLE001
        print(f"请求失败：{e}")
        return 1

    show(1, r)
    if r["errs"]:
        return 1

    # 自动批准，最多来回 6 次（一个回合可能触发多次工具审批）
    for i in range(2, 8):
        if not r["approvals"]:
            break
        aid = r["approvals"][0][0]
        print(f"\n✅ 自动批准 {aid}")
        r = stream("/api/approve", {"approvalId": aid, "approved": True})
        show(i, r)
        if r["errs"]:
            return 1

    print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
