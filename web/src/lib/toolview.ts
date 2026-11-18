/**
 * 工具调用面板（消息列表里点开工具那一行的底部面板）该显示什么。
 *
 * **原则：模型看到什么，这里就显示什么。** 工具结果是一段纯文本（退出码、截断、
 * 续读位置都写在正文里，见 core 的 `ToolOutcome`），面板原样显示，不另拆字段。
 *
 * 参数：主行只放最关键的那个（命令 / 路径 / 网址），其余参数列在下面。
 *
 * 纯函数，不碰 DOM —— MessageList 只管照着画。
 */

import type { ToolPart } from "./sse";

/** 没有专门样式的参数：原样列出来。`block` = 多行 / 很长，用代码块显示 */
export type Field = { key: string; text: string; block: boolean };

export type InputView = {
  /** 主行：命令 / 路径 / 网址 …（可能为空） */
  primary: string;
  /** 主行之外的参数 */
  params: Field[];
};

export type ResultView = {
  /** 模型收到的原文 */
  text: string;
  failed: boolean;
  /** 原文是 unified diff（edit 的结果）：按行染色 */
  diff: boolean;
  /** 工具返回的图片（沙箱里的原图路径；模型看到的是缩放后的那份） */
  images: string[];
};

const str = (v: unknown) => (typeof v === "string" ? v : "");

/**
 * 这次工具调用是不是**失败**了 —— 只看正文里模型也看得见的信号：
 *   - 以 `错误：` 开头：核心出错时的唯一写法（read / bash / edit 都这样）
 *   - 末尾的 `[exit N]` 不是 0（超时被杀也是非 0）
 *
 * 折叠行染红和面板用**同一个**判定。
 */
export function failed(t: ToolPart): boolean {
  // 首屏服务端只给一个布尔（`failed`）—— output 正文不再传（见 core 的
  // `OUTPUT_ON_WIRE`）。有它就信它，这是服务端按全文算的，比拿截断的那点
  // 自己判可靠（截断会把末尾的 `[exit 1]` 切掉）。
  if (t.failed !== undefined) return t.failed;
  // 没有那个字段的（流式过程中、老的本地数据）：退回自己判
  const text = t.output;
  if (text === undefined) return false;
  if (text.startsWith("错误：")) return true;
  const exit = /\[exit (-?\d+)\]\s*$/.exec(text);
  return exit !== null && exit[1] !== "0";
}

/** 一个值怎么显示：字符串原样，别的排成 JSON */
function field(key: string, v: unknown): Field {
  const text =
    typeof v === "string" ? v : (JSON.stringify(v, null, 2) ?? String(v));
  const block =
    typeof v === "object" && v !== null
      ? true
      : text.includes("\n") || text.length > 80;
  return { key, text, block };
}

/** 没被消费掉的字段 → Field[]（保持原来的顺序） */
function rest(o: Record<string, unknown>, used: Set<string>): Field[] {
  return Object.entries(o)
    .filter(([k]) => !used.has(k))
    .map(([k, v]) => field(k, v));
}

// ---------------------------------------------------------------------------
// 参数
// ---------------------------------------------------------------------------

/** edit 的 edits：每处一段，旧文 `-`、新文 `+`（比转义过的 JSON 好读得多） */
function formatEdits(edits: unknown): string {
  if (!Array.isArray(edits)) return JSON.stringify(edits, null, 2);
  return edits
    .map((e, i) => {
      const o = (e ?? {}) as Record<string, unknown>;
      const lines = (s: unknown, mark: string) =>
        str(s)
          .split("\n")
          .map((l) => `${mark} ${l}`)
          .join("\n");
      return `── 第 ${i + 1} 处 ──\n${lines(o.oldText, "-")}\n${lines(o.newText, "+")}`;
    })
    .join("\n");
}

// ---------------------------------------------------------------------------
// 工具名 → 展示信息
// ---------------------------------------------------------------------------

/**
 * 一个工具在界面上怎么说。**只在这一处**：摘要行（`summarize`）、面板标题
 * （`toolTitle`）、左侧图标（`toolIcon`）、面板主行（`describeInput`）以前各自
 * `switch (t.toolName)` 一遍 —— 加一个工具要改四处，漏一处就是「摘要对了图标不对」。
 *
 * `icon` 的取值必须是 `Icon.svelte` 的 `Name`（写错了编译期就报）。
 */
export type ToolFace = {
  /** 折叠行左侧的图标 */
  icon: "file" | "newfile" | "terminal" | "pencil" | "clock";
  /** 面板标题（要短，细节放正文） */
  title: string;
  /**
   * 折叠行那一句摘要。`a` 是**可能不完整**的参数（参数还在流时从半截 JSON 抠的）。
   * 只取第一行，长度交给 CSS 截断 —— 屏幕多宽就显示多少。
   */
  summary: (a: Record<string, unknown>) => string;
  /** 面板主行那个最关键的值（命令 / 路径 / 网址）取哪个字段 */
  primary?: string;
};

/** `edit` 这次是新建文件（`content`）还是改已有文件（`edits`） */
export const isNewFile = (a: Record<string, unknown>) =>
  a.content !== undefined;

const firstLine = (v: unknown) =>
  (typeof v === "string" ? v : "").split("\n")[0].trim();

const pathOr = (a: Record<string, unknown>, fallback: string) =>
  str(a.path) || fallback;

/** 认识的工具。不认识的走 `FACE_FALLBACK`（以后核心加了新工具，是「多了个没见过的
 *  图标」，而不是「这一行空了」）。 */
const FACES: Record<string, ToolFace> = {
  read: {
    icon: "file",
    title: "读取文件",
    primary: "path",
    summary: (a) => `读取 ${pathOr(a, "文件")}`,
  },
  bash: {
    icon: "terminal",
    title: "执行命令",
    primary: "command",
    summary: (a) => firstLine(a.command) || "执行命令",
  },
  edit: {
    // 图标和说法都看 `content` / `edits`（核心用这两个参数区分新建 / 改写，
    // 见 `edit_mode`）—— 两个都给是报错的用法，认 `content` 就行（那一行也会
    // 是红的）。所以 `edit` 的 icon / title / summary 是函数，不是常量。
    icon: "pencil",
    title: "编辑文件",
    primary: "path",
    summary: (a) =>
      isNewFile(a) ? `新建 ${pathOr(a, "文件")}` : `编辑 ${pathOr(a, "文件")}`,
  },
};

/** 不认识的工具：时钟图标 + 名字本身 */
const FACE_FALLBACK: ToolFace = {
  icon: "clock",
  title: "",
  summary: () => "",
};

/** 这个工具的展示信息（`edit` 的图标 / 标题要按参数再判一次） */
export function faceOf(
  t: ToolPart,
  args: Record<string, unknown>,
): ToolFace {
  const f = FACES[t.toolName];
  if (!f) {
    const name = t.toolName || "工具调用";
    return { ...FACE_FALLBACK, title: name, summary: () => name };
  }
  if (t.toolName !== "edit") return f;
  // 新建：图标换成 newfile，标题也换
  return isNewFile(args) ? { ...f, icon: "newfile", title: "新建文件" } : f;
}

export function describeInput(
  t: ToolPart,
  args: Record<string, unknown>,
): InputView {
  const a = args;
  const used = new Set<string>();
  const key = faceOf(t, a).primary;
  const primary = key ? str(a[key]) : "";
  if (key) used.add(key);
  // `edit` 改成功了，结果里的 diff 就是同一份改动，不必再列一遍旧文 / 新文
  if (t.toolName === "edit" && t.output !== undefined && isDiff(t.output))
    used.add("edits");

  const params = rest(a, used).map((f) =>
    f.key === "edits" && t.toolName === "edit"
      ? { key: f.key, text: formatEdits(a.edits), block: true }
      : f,
  );
  return { primary, params };
}

// ---------------------------------------------------------------------------
// 结果
// ---------------------------------------------------------------------------

/** edit 成功时的结果就是一段 unified diff（`--- 路径` / `+++ 路径` 开头） */
function isDiff(text: string): boolean {
  return text.startsWith("--- ") && text.includes("\n+++ ");
}

/**
 * `output` 覆盖：面板里取到全文之后要拿**全文**重算 `failed` / `diff`
 * （否则一个 400 字之后才出现 `[exit 1]` 的命令会被判成成功）。
 *
 * 用参数而**不是**在调用方 `{ ...t, output }` 克隆一个 ToolPart：模板里那个
 * 浅拷贝每次渲染都跑一遍，而它只是为了换一个字段。
 */
export function describeResult(
  t: ToolPart,
  output = t.output,
): ResultView | null {
  if (output === undefined) return null;
  return {
    text: output,
    failed: failed({ ...t, output }),
    diff: isDiff(output),
    images: t.images ?? [],
  };
}
