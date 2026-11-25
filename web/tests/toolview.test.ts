/**
 * `lib/toolview.ts` 的 `faceOf`：工具名 → 摘要 / 图标 / 面板标题。
 *
 * 这个映射以前在 `MessageList.svelte` 里写了三遍（`summarize` / `toolTitle` /
 * `toolIcon`），加上 `describeInput` 里一遍 = 四份。现在只有这一处，加工具改一个
 * 地方 —— 这几条测试就是那个「一个地方」的规格。
 */

import { describe, expect, test } from "bun:test";
import { describeInput, faceOf, isNewFile } from "../src/lib/toolview.ts";
import type { ToolPart } from "../src/lib/sse";

const tool = (name: string, input: unknown = {}): ToolPart => ({
  type: "tool",
  toolCallId: "c1",
  toolName: name,
  inputText: "",
  input,
  state: "output",
});

describe("faceOf", () => {
  test("read：文件图标 + 路径", () => {
    const f = faceOf(tool("read", { path: "/root/a.txt" }), {
      path: "/root/a.txt",
    });
    expect(f.icon).toBe("file");
    expect(f.title).toBe("读取文件");
    expect(f.summary({ path: "/root/a.txt" })).toBe("读取 /root/a.txt");
  });

  test("bash：终端图标 + 命令第一行", () => {
    const f = faceOf(tool("bash"), {});
    expect(f.icon).toBe("terminal");
    expect(f.title).toBe("执行命令");
    expect(f.summary({ command: "apk add py\n&& echo hi" })).toBe("apk add py");
  });

  test("edit 新建（有 content）：newfile 图标 + 「新建」", () => {
    const a = { path: "/root/n.md", content: "x" };
    const f = faceOf(tool("edit", a), a);
    expect(f.icon).toBe("newfile");
    expect(f.title).toBe("新建文件");
    expect(f.summary(a)).toBe("新建 /root/n.md");
  });

  test("edit 改写（只有 edits）：pencil 图标 + 「编辑」", () => {
    const a = { path: "/root/n.md", edits: [] };
    const f = faceOf(tool("edit", a), a);
    expect(f.icon).toBe("pencil");
    expect(f.title).toBe("编辑文件");
    expect(f.summary(a)).toBe("编辑 /root/n.md");
  });

  test("不认识的工具：时钟图标 + 名字本身（不能空一行）", () => {
    const f = faceOf(tool("some_new_tool"), {});
    expect(f.icon).toBe("clock");
    expect(f.title).toBe("some_new_tool");
    expect(f.summary({})).toBe("some_new_tool");
  });

  test("参数缺失：摘要不能崩，给个兜底词", () => {
    const f = faceOf(tool("read"), {});
    expect(f.summary({})).toBe("读取 文件");
  });
});

describe("describeInput 和 faceOf 用同一个 primary", () => {
  test("read / bash / edit 的主行就是 faceOf.primary 那个字段", () => {
    for (const [name, key] of [
      ["read", "path"],
      ["bash", "command"],
      ["edit", "path"],
    ] as const) {
      const a = { path: "P", command: "C", edits: [] };
      const v = describeInput(tool(name), a);
      expect(faceOf(tool(name), a).primary).toBe(key);
      expect(v.primary).toBe(a[key]);
    }
  });
});

describe("isNewFile", () => {
  test("看 content（核心用 content / edits 区分新建和改写）", () => {
    expect(isNewFile({ content: "" })).toBe(true);
    expect(isNewFile({ edits: [] })).toBe(false);
    expect(isNewFile({})).toBe(false);
  });
});
