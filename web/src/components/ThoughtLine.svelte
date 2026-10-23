<script lang="ts">
  /**
   * 思考摘要行：模型此刻在想什么。
   *
   * 流式时**每写满一行就切换**：按这一行的实际宽度把正在写的段落折行，
   * 最近一条写满的行一出来就显示；遇到换行，那一段的最后一行也算写满。
   * 正在写的半行不显示 —— 逐字刷新会让这一行一直抖，读不进去。
   * 只在换行时切换的话，模型一段思考常常几百字不换行，要等整段
   * 写完才看到开头，太滞后。按行切换像提词器：一行一行往上走。
   *
   * 结束后显示最后一段（从段首开始，放不下的省略）。
   *
   * 节奏：每行至少停留 HOLD 毫秒。快模型一秒能出好几行，每行都切的话
   * 亮起动画还没走完就被打断，看着是在闪。停留期间来的新行只记最新的
   * 一条，到点直接切过去，中间的跳过。
   *
   * 两头不等：
   * - **开头**：还没有写满的行时，正在写的那半行直接显示、原地变长（不重播亮起）。
   *   不然要等二三十个字写满一行才有东西，看着像模型还没开始想。
   * - **结尾**：思考一结束（后面接上了正文或工具调用）就立刻换成最终那一行，不等
   *   停留、不重播亮起 —— 不然工具调用都跑完了，这一行的动画还没收住。
   */
  import { untrack } from "svelte";

  let { text, live }: { text: string; live: boolean } = $props();

  const HOLD = 500;

  /** 这一行可用的宽度（像素） */
  let width = $state(0);
  let box: HTMLSpanElement | null = $state(null);

  // —— 增量折行 ——
  //
  // 流式时每个 delta 都会走到这里，思考可能有几十 KB。只重算"正在写的
  // 那一行"：rowStart 之前的都已经折好了，不用再量。
  let rowStart = 0;
  let seenLen = 0;
  let seenWidth = 0;
  let lastRow = "";

  /**
   * 该显示什么，以及怎么换上去：
   * - `row`：写满的一行 —— 守停留时间、换上去时重播亮起；
   * - `partial`：第一行还没写满，正在写的那半行 —— 原地更新；
   * - `final`：思考结束了 —— 立刻换，原地更新。
   */
  type Line = { text: string; kind: "row" | "partial" | "final" };

  // 留几像素余量：canvas 量的和实际排版有细微出入，宁可早一点换行也别被省略号截掉
  const latest = $derived<Line>(
    live ? follow(text, width - 4) : { text: finalLine(text), kind: "final" },
  );

  // 历史消息首帧就是最终内容，直接显示、不播亮起动画
  let shown = $state(untrack(() => latest.text));
  /**
   * 换「写满的一行」的次数。元素按它重建（`{#key}`）—— 亮起动画靠重建才重播，
   * 而原地更新（半行变长、结束时换成最终那行）不重建：重建的话流光也跟着从头来，
   * 每来一个字跳一下。
   */
  let rows = $state(0);
  let since = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;

  $effect(() => {
    const next = latest;
    if (next.text === shown) return;
    clearTimeout(timer);
    if (next.kind !== "row") {
      shown = next.text;
      return;
    }
    const wait = since + HOLD - performance.now();
    const swap = () => {
      const now = latest; // 等的这一会儿可能又变了（甚至已经结束了）
      shown = now.text;
      if (now.kind !== "row") return;
      rows++;
      since = performance.now();
    };
    if (wait <= 0) swap();
    else timer = setTimeout(swap, wait);
  });
  $effect(() => () => clearTimeout(timer));

  /** 流式：最近一条写满的行；一行都还没写满时是正在写的那半行 */
  function follow(t: string, w: number): Line {
    if (w <= 0) return { text: lastParagraph(t, true), kind: "row" };
    // 文本变短（换了一条消息）或宽度变了：从头折
    if (t.length < seenLen || w !== seenWidth) {
      rowStart = 0;
      lastRow = "";
      seenWidth = w;
    }
    seenLen = t.length;

    while (rowStart < t.length) {
      const nl = t.indexOf("\n", rowStart);
      const end = nl === -1 ? t.length : nl;
      const starts = wrap(t.slice(rowStart, end), w);
      const rowOf = (k: number) =>
        clean(
          t.slice(
            rowStart + starts[k],
            rowStart + (starts[k + 1] ?? end - rowStart),
          ),
        );

      if (nl === -1) {
        // 段落还在写：除最后一行外都写满了
        if (starts.length >= 2) {
          const r = rowOf(starts.length - 2);
          if (r) lastRow = r;
          rowStart += starts[starts.length - 1];
        }
        break;
      }
      // 段落写完了：最后一行也算写满
      const r = starts.length ? rowOf(starts.length - 1) : "";
      if (r) lastRow = r;
      rowStart = nl + 1;
    }
    if (lastRow) return { text: lastRow, kind: "row" };
    // 一行都还没写满：正在写的那半行（rowStart 之后、下一个换行之前）
    const nl = t.indexOf("\n", rowStart);
    return {
      text: clean(t.slice(rowStart, nl === -1 ? t.length : nl)),
      kind: "partial",
    };
  }

  /** 结束后：最后一段，从段首开始 */
  function finalLine(t: string): string {
    return lastParagraph(t, false);
  }

  /** 从末尾往前找第一段非空的。streaming 时只看已经换行的段 */
  function lastParagraph(t: string, streaming: boolean): string {
    let end = streaming ? t.lastIndexOf("\n") : t.length;
    while (end > 0) {
      const start = t.lastIndexOf("\n", end - 1) + 1;
      const l = clean(t.slice(start, end));
      if (l) return l;
      end = start - 1;
    }
    return "";
  }

  function clean(l: string): string {
    return l
      .replace(/^\s*(#+|[-*+]|\d+[.)])\s+/, "")
      .replace(/\*\*|__|`/g, "")
      .trim();
  }

  // —— 量字宽 ——
  let ctx: CanvasRenderingContext2D | null = null;
  let font = "";
  // 中日韩字符一个字一个单位（可以在任意字之间断行），其他按单词
  const TOKEN = /[⺀-鿿豈-﫿＀-￯　-〿]|\s+|[^\s⺀-鿿豈-﫿＀-￯　-〿]+/g;

  /** 贪心折行。返回每一行在 s 里的起点 */
  function wrap(s: string, w: number): number[] {
    if (!ctx) {
      ctx = document.createElement("canvas").getContext("2d");
      if (!ctx) return [0];
    }
    if (box && !font) font = getComputedStyle(box).font;
    ctx.font = font;
    const starts = [0];
    let cw = 0;
    let hasInk = false;
    TOKEN.lastIndex = 0;
    for (let m = TOKEN.exec(s); m; m = TOKEN.exec(s)) {
      const tok = m[0];
      const space = /^\s/.test(tok);
      const tw = ctx.measureText(tok).width;
      if (!space && hasInk && cw + tw > w) {
        starts.push(m.index);
        cw = tw;
        continue;
      }
      if (space && !hasInk) continue; // 行首空白不占宽
      cw += tw;
      if (!space) hasInk = true;
    }
    return starts;
  }
</script>

<!-- 这里两层 span 不能合：外层是**稳定**的 flex 项（量宽度 clientWidth、
     取字体），内层被 {#key} 重建 —— 每换一行写满的重建一次元素，
     `.reveal` 那条亮起动画才会重播。合成一个元素就只剩“量得到”或
     “能重播”两者之一。 -->
<span
  bind:this={box}
  bind:clientWidth={width}
  class="flex min-w-0 flex-1 text-sm"
>
  {#key rows}
    <span class="min-w-0 truncate text-muted" class:live class:reveal={rows > 0}
      >{shown || (live ? "思考中…" : "思考")}</span
    >
  {/key}
</span>

<style>
  /*
   * 每换一行（{#key} 重建元素）从左到右亮起：遮罩是一条带软边的渐变，
   * 从右往左扫过去，字就一个接一个浮现，边缘是虚的所以不生硬。
   * 还在想的时候再叠一层流光（亮带循环扫过文字）。
   * 只有这一行在动，重绘面积很小。
   */
  .reveal {
    -webkit-mask-image: linear-gradient(
      90deg,
      #000 0 33%,
      transparent 66% 100%
    );
    mask-image: linear-gradient(90deg, #000 0 33%, transparent 66% 100%);
    -webkit-mask-size: 300% 100%;
    mask-size: 300% 100%;
    -webkit-mask-repeat: no-repeat;
    mask-repeat: no-repeat;
    animation: thought-reveal 450ms cubic-bezier(0.25, 0.6, 0.3, 1) both;
  }
  .live {
    background: linear-gradient(
        90deg,
        var(--color-muted) 0 40%,
        var(--color-fg) 50%,
        var(--color-muted) 60% 100%
      )
      0 0 / 250% 100%;
    -webkit-background-clip: text;
    background-clip: text;
    color: transparent;
    animation: thought-shimmer 2.4s linear infinite;
  }
  .reveal.live {
    animation:
      thought-reveal 450ms cubic-bezier(0.25, 0.6, 0.3, 1) both,
      thought-shimmer 2.4s linear infinite;
  }
  @keyframes thought-reveal {
    from {
      -webkit-mask-position: 100% 0;
      mask-position: 100% 0;
    }
    to {
      -webkit-mask-position: 0 0;
      mask-position: 0 0;
    }
  }
  @keyframes thought-shimmer {
    from {
      background-position: 100% 0;
    }
    to {
      background-position: 0 0;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .reveal,
    .live {
      animation: none;
      -webkit-mask-image: none;
      mask-image: none;
    }
  }
</style>
