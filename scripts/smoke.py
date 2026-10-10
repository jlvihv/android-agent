#!/usr/bin/env python3
"""端到端冒烟测试：真实 LLM + 审批流。

前提：core 已在 8787 跑着，且配好了 provider 环境变量。

    just dev                 # 另一个终端
    python3 scripts/smoke.py

它会走完整条链路：
    用户请求 → 模型读界面（只读，自动执行）→ 模型请求点击（需审批）
    → 批准 → 设备执行 → 新快照 → 模型复述结果
"""

import json
import sys
import urllib.request

import os
BASE = os.environ.get("AGENT_BASE", "http://127.0.0.1:8787")


def stream(path: str, body: dict) -> dict:
    """读 SSE，把 chunk 折叠成可读转录。"""
    req = urllib.request.Request(
        BASE + path,
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json"},
    )
    text, reasoning, tools, approvals, errs = "", "", {}, [], []
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
                            "approvals": approvals,
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


def show(tag: str, r: dict) -> dict:
    print(f"\n{'=' * 68}\n{tag}\n{'=' * 68}")
    if r["reasoning"]:
        print(f"[思考] {r['reasoning'][:220]}")
    if r["text"]:
        print(f"[回答] {r['text'][:500]}")
    for _cid, (name, inp, out) in r["tools"].items():
        state = "已执行" if out is not None else "未执行"
        print(f"[工具] {name}({json.dumps(inp, ensure_ascii=False)}) -> {state}")
        if out and name == "device_snapshot":
            print(f"       {str(out.get('text', ''))[:160]}...")
    for aid, _cid, reason in r["approvals"]:
        print(f"[待批] {aid} :: {reason}")
    for e in r["errs"]:
        print(f"[错误] {e}")
    return r


def main() -> int:
    try:
        r1 = show(
            "第 1 轮：用户请求",
            stream("/api/chat", {"text": "看看现在屏幕上有什么，然后点一下「打开设置」"}),
        )
    except Exception as e:  # noqa: BLE001
        print(f"请求失败：{e}\n（core 起来了吗？provider 配了吗？）")
        return 1

    if r1["errs"]:
        return 1
    if not r1["approvals"]:
        print("\n❌ 没有产生审批请求 —— 模型没有按预期请求 device_tap")
        return 1

    aid, tap_call = r1["approvals"][0][0], r1["approvals"][0][1]
    r2 = show(
        f"第 2 轮：批准 {aid}",
        stream("/api/approve", {"approvalId": aid, "approved": True}),
    )

    if r2["errs"]:
        return 1

    # 批准后 tap 必须真的执行，而且结果里要带上**操作后的新界面**
    out = r2["outputs"].get(tap_call)
    if out is None:
        print("\n❌ 批准后 device_tap 没有执行")
        return 1
    if "settings" not in json.dumps(out, ensure_ascii=False).lower():
        print(f"\n❌ tap 结果里没有操作后的新界面（模型会拿旧画面脉猜）：{out}")
        return 1

    print("\n✅ 全链路通过：读界面 → 请求点击 → 审批 → 执行 → 带新界面回灌 → 复述")
    return 0


if __name__ == "__main__":
    sys.exit(main())
