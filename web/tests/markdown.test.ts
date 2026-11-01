/**
 * `markdown.ts` 里**裸 HTML** 那两条路的测试。
 *
 * 这块的边界全在「什么时候算一整个块」上，而这句话只有跑一遍才知道对不对：
 *   - 空行在 CommonMark 里是 HTML 块的硬边界，但模型写 `<details>` / 两栏 div
 *     时会跨空行 —— 按标签层数收才不会剩下半截；
 *   - 流式输出到一半的 `<spa` 不能当成 HTML，否则整段会短暂消失；
 *   - `<https://…>` 自动链接跟行内标签抢同一个 `<`。
 *
 * `markdown.ts` 依赖 `store.svelte.ts`（要 `store.sandboxKey`），而那些 runes
 * 在 bun 里不存在 —— 所以下面先给三个同名全局，再动态 import。
 * 纯前端、不上 DOM，毫秒级，跟 `tests/` 里另外两个一个待遇。
 *
 *     bun test            # 或者 `just web test`
 */

import { describe, expect, test } from "bun:test";

// @ts-expect-error 运行时补的桩，Svelte 编译时才有这两个 rune
globalThis.$state = <T>(v: T) => v;
// @ts-expect-error 同上
globalThis.$derived = { by: (f: () => unknown) => f() };
// @ts-expect-error store 构造时读 location.hostname（判「本机」）
globalThis.location = {
  hostname: "127.0.0.1",
  port: "",
  protocol: "http:",
  origin: "http://127.0.0.1",
};

const { parse, inline } = await import("../src/lib/markdown.ts");

/** 块列表压成 `t` 序列，断言里一眼能看出结构（`html` 带内容，其余只报类型） */
function kinds(src: string): string[] {
  return parse(src).map((b) =>
    b.t === "html" ? `html:${b.v}` : b.t === "code" ? `code:${b.lang}` : b.t,
  );
}

describe("裸 HTML 块", () => {
  test("容器没闭合时，空行不结束块", () => {
    const src = [
      '<div class="card">',
      "",
      "里面 **不加粗**",
      "",
      "第二段",
      "</div>",
    ].join("\n");
    expect(kinds(src)).toEqual([`html:${src}`]);
  });

  test("闭合之后，紧跟的标签并进同一块，否则回车就是段落", () => {
    expect(kinds("<div>a</div>\n\n后面")).toEqual(["html:<div>a</div>", "p"]);
    expect(kinds("<div>a</div>\n<b>b</b>")).toEqual([
      "html:<div>a</div>\n<b>b</b>",
    ]);
  });

  test("嵌套 + 空行都在里面", () => {
    const src =
      "<details>\n<summary>标题</summary>\n\n内容\n\n- 列表项\n</details>";
    expect(kinds(src)).toEqual([`html:${src}`]);
  });

  test("表里的内容不会被当成 markdown 解析", () => {
    const src = "<table>\n<tr><td>**粗**</td></tr>\n</table>";
    expect(kinds(src)).toEqual([`html:${src}`]);
  });

  test("半截标签不算块（流式到一半不能吞掉内容）", () => {
    expect(kinds("<spa 还在写")).toEqual(["p"]);
    expect(kinds("前文\n<div 之后")).toEqual(["p"]);
  });

  test("围栏里的 HTML 还是代码", () => {
    expect(kinds("```html\n<div>x</div>\n```")).toEqual(["code:html"]);
  });

  test("相邻的 HTML 块和段落互不影响", () => {
    expect(kinds("段落\n\n<div>x</div>\n\n段落二")).toEqual([
      "p",
      "html:<div>x</div>",
      "p",
    ]);
  });
});

describe("交了一整页 HTML 时剥回片段", () => {
  const texts = (src: string) =>
    parse(src)
      .filter((b) => b.t === "html")
      .map((b) => (b as { v: string }).v);

  test("DOCTYPE / html / head / body 外壳全剥掉", () => {
    expect(
      texts(
        "<!DOCTYPE html>\n<html>\n<body>\n<div>内容</div>\n</body>\n</html>",
      ),
    ).toEqual(["<div>内容</div>"]);
    expect(texts("<!doctype html>\n<div>内容</div>")).toEqual([
      "<div>内容</div>",
    ]);
  });

  test("`<style>` / `<script>` 必须剥 —— 它们是全局作用域", () => {
    // 实测：一条 `body{display:none}` 在 .raw 里能把**整个 App** 弄成白屏，
    // 一条 `*{color:red}` 能把所有文字染色。剥掉只是「样式没了」，不会整页崩。
    expect(texts("<style>body{display:none}</style>\n<div>内容</div>")).toEqual(
      ["<div>内容</div>"],
    );
    expect(texts("<div><script>alert(1)</script>内容</div>")).toEqual([
      "<div>内容</div>",
    ]);
    expect(texts("<div><style>.a{color:red}</style>x</div>")).toEqual([
      "<div>x</div>",
    ]);
  });

  test("普通片段一点不动（含 inline style）", () => {
    expect(texts('<div style="display:grid">\n<span>a</span>\n</div>')).toEqual(
      ['<div style="display:grid">\n<span>a</span>\n</div>'],
    );
  });
});

describe("行内不认 HTML（那是块的事）", () => {
  test("代码里的尖括号不会被当成标签吞掉", () => {
    // 这两条是实测出来的：以前行内认标签，`Array<T>` 的 `<T>` 被当标签渲染成空，
    // `x<y>z` 直接吞掉 x 和 z —— 对天天聊代码的助手来说是每天都踩的坑
    expect(inline("Array<T> 泛型")).toEqual([
      { t: "text", v: "Array<T> 泛型" },
    ]);
    expect(inline("x<y>z")).toEqual([{ t: "text", v: "x<y>z" }]);
  });

  test("`<https://…>` 自动链接照旧是链接", () => {
    expect(inline("<https://a.com/x>")).toEqual([
      {
        t: "link",
        href: "https://a.com/x",
        c: [{ t: "text", v: "https://a.com/x" }],
      },
    ]);
  });

  test("行内的 HTML 标签原样留在文字里（要排版就写块）", () => {
    expect(inline("按 <kbd>⌘</kbd> 保存")).toEqual([
      { t: "text", v: "按 <kbd>⌘</kbd> 保存" },
    ]);
  });
});
