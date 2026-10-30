<script lang="ts">
  /**
   * 渲染 markdown 块。走 Svelte 组件而不是 {@html}：
   * 没有 XSS 面，样式也全在 Tailwind 里。
   *
   * `base`：这段文字**来源文件**的目录（查看器渲染 `.md` 时给，如 `/root/docs/`），
   * 文件里的相对链接按它解析；消息里不传。
   *
   * 列表、引用可以嵌套，所以块和行内都是递归 snippet。
   */
  import { untrack } from "svelte";
  import Img from "./Img.svelte";
  import {
    parse,
    sandboxUrl,
    mediaKind,
    type Block,
    type Span,
    type Align,
  } from "../lib/markdown";
  import { openViewer } from "../lib/viewer.svelte.ts";

  let { text, base = "" }: { text: string; base?: string } = $props();

  // 流式时每个 delta 都会改 text，一帧里可能来好几个。整段重新解析 +
  // 重新渲染一帧做一次就够了：变化先攒着，下一帧取最新值。
  // 历史消息首帧就是完整的，不经过这里。
  let shown = $state(untrack(() => text));
  let raf = 0;
  $effect(() => {
    void text;
    raf ||= requestAnimationFrame(() => {
      raf = 0;
      shown = text;
    });
  });
  $effect(() => () => cancelAnimationFrame(raf));

  const doc = $derived(parse(shown, base));

  const HEADING = [
    "",
    "text-xl",
    "text-lg",
    "text-base",
    "text-base",
    "text-sm",
    "text-sm",
  ];
  const ALIGN: Record<string, string> = {
    left: "text-left",
    center: "text-center",
    right: "text-right",
  };
  const al = (a: Align) => (a ? ALIGN[a] : "text-left");

  /** 刚复制的那个代码块，显示"已复制"一会儿 */
  let copied = $state<string | null>(null);

  async function copy(v: string) {
    try {
      await navigator.clipboard.writeText(v);
    } catch {
      // 剪贴板 API 被拒（WebView 没焦点之类）：退回老办法
      const ta = document.createElement("textarea");
      ta.value = v;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      document.execCommand("copy");
      ta.remove();
    }
    copied = v;
    setTimeout(() => copied === v && (copied = null), COPIED_MS);
  }
  /** 「已复制」这个提示挂多久（ms）——够看清，又不至于一直杵在那儿 */
  const COPIED_MS = 1500;
</script>

{#snippet spans(list: Span[])}
  {#each list as s}
    {#if s.t === "text"}{s.v}
    {:else if s.t === "code"}<code
        class="rounded bg-control px-1 py-px font-mono text-[0.88em] break-words"
        >{s.v}</code
      >
    {:else if s.t === "strong"}<strong class="font-semibold"
        >{@render spans(s.c)}</strong
      >
    {:else if s.t === "em"}<em class="italic">{@render spans(s.c)}</em>
    {:else if s.t === "del"}<del class="text-muted">{@render spans(s.c)}</del>
    {:else if s.t === "img" && mediaKind(s.src) !== "image"}
      <!-- 音视频：图片语法嵌进来，显示成播放器。preload=metadata 只拉开头，
           服务端支持 Range，拖进度条不用整个下载 -->
      {@const src = sandboxUrl(s.src) ?? s.src}
      {#if mediaKind(s.src) === "video"}
        <!-- svelte-ignore a11y_media_has_caption -->
        <video
          {src}
          controls
          playsinline
          preload="metadata"
          class="my-1 block max-h-80 w-full rounded-xl bg-black"
        ></video>
      {:else}
        <span class="my-1 flex flex-col gap-1">
          {#if s.alt}<span class="text-sm text-muted">{s.alt}</span>{/if}
          <audio {src} controls preload="metadata" class="w-full"></audio>
        </span>
      {/if}
    {:else if s.t === "img"}
      {@const local = sandboxUrl(s.src)}
      <!-- 沙箱里的图片点开看大图；外链图片只内嵌 -->
      <button
        class="my-1 block max-w-full overflow-hidden rounded-xl bg-hairline-soft align-top"
        disabled={!local}
        onclick={() => local && openViewer(local, s.alt)}
      >
        <Img
          src={local ?? s.src}
          alt={s.alt}
          loading="lazy"
          class="max-h-80 max-w-full object-contain"
        />
      </button>
    {:else}
      {@const local = sandboxUrl(s.href)}
      {#if local}
        <!-- 沙箱文件：在 App 里的查看器打开，不跳走 -->
        <a
          href={local}
          onclick={(e) => {
            e.preventDefault();
            openViewer(local);
          }}
          class="break-all text-fg underline decoration-muted underline-offset-2"
          >{@render spans(s.c)}</a
        >
      {:else}<a
          href={s.href}
          target="_blank"
          rel="noopener noreferrer"
          class="break-all underline decoration-faint underline-offset-2"
          >{@render spans(s.c)}</a
        >
      {/if}
    {/if}
  {/each}
{/snippet}

{#snippet blocks(list: Block[])}
  {#each list as b}
    {#if b.t === "p"}
      <p class="whitespace-pre-wrap break-words">{@render spans(b.spans)}</p>
    {:else if b.t === "h"}
      <p
        class="font-semibold {HEADING[b.level]} {b.level >= 4
          ? 'text-muted'
          : ''} {b.level <= 2 ? 'mt-1' : ''}"
      >
        {@render spans(b.spans)}
      </p>
    {:else if b.t === "hr"}
      <hr class="my-1 border-hairline" />
    {:else if b.t === "code"}
      <div class="overflow-hidden rounded-xl bg-hairline-soft">
        <div
          class="flex items-center border-b border-hairline px-3 py-1 text-meta text-faint"
        >
          <span>{b.lang || "代码"}</span>
          <button
            class="-mr-1.5 ml-auto px-1.5 py-0.5"
            onclick={() => copy(b.v)}
          >
            {copied === b.v ? "已复制" : "复制"}
          </button>
        </div>
        <pre
          class="overflow-x-auto px-3 py-2.5 font-mono text-code leading-5">{b.v}</pre>
      </div>
    {:else if b.t === "table"}
      <!-- 宽表格在自己的框里横向滚动，不撑破页面 -->
      <div class="-mx-1 overflow-x-auto px-1">
        <table class="w-max min-w-full border-collapse text-sm leading-snug">
          <thead>
            <tr class="border-b border-control">
              {#each b.head as cell, k}
                <th class="px-2.5 py-1.5 font-semibold {al(b.align[k])}"
                  >{@render spans(cell)}</th
                >
              {/each}
            </tr>
          </thead>
          <tbody>
            {#each b.rows as row}
              <tr class="border-b border-hairline last:border-0">
                {#each row as cell, k}
                  <td class="px-2.5 py-1.5 align-top {al(b.align[k])}"
                    >{@render spans(cell)}</td
                  >
                {/each}
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {:else if b.t === "quote"}
      <blockquote
        class="flex flex-col gap-2 border-l-2 border-control pl-3 text-muted"
      >
        {@render blocks(b.blocks)}
      </blockquote>
    {:else if b.t === "list"}
      {@const tag = b.ordered ? "ol" : "ul"}
      <svelte:element
        this={tag}
        start={b.ordered && b.start !== 1 ? b.start : undefined}
        class="flex flex-col pl-5 {b.tight ? 'gap-1' : 'gap-2.5'}
               {b.ordered ? 'list-decimal' : 'list-disc'} marker:text-muted"
      >
        {#each b.items as it}
          <li class={it.task === null ? "" : "list-none -ml-5"}>
            <!-- 复选框那一行才需要 flex 行（图标 + 正文并排）。普通项直接放正文：
                 之前无论有没有复选框都套着这层 `flex gap-2`，而它对普通项是**完全
                 透明的一层**（浏览器里实测：把它改成 display:contents，整页每个
                 元素的盒子一个像素都不动）。见 docs/07-debugging.md 的 DOM 审计。 -->
            {#if it.task !== null}
              <div class="flex gap-2">
                <span
                  class="mt-[0.3em] grid size-[1.05em] shrink-0 place-items-center rounded-[0.3em] border
                         {it.task
                    ? 'border-muted bg-muted text-ink'
                    : 'border-faint'}"
                >
                  {#if it.task}
                    <svg viewBox="0 0 12 12" class="size-[0.75em]"
                      ><path
                        d="M2.5 6.2 5 8.6l4.5-5"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.8"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                      /></svg
                    >
                  {/if}
                </span>
                {@render itemBody(it.blocks, b.tight)}
              </div>
            {:else if it.blocks.length === 1 && it.blocks[0].t === "p"}
              <!-- 单段落的普通项：**不套 itemBody**。它那层只管两件事 ——
                   复选框行的 `flex-1`、多块项之间的 `gap` —— 这里一件都用不上
                   （拔掉测试里就是那 15 个 `li > div.flex > p` 候选）。
                   直接渲染，`<li>` 里就是一个 `<p>`。 -->
              {@render blocks(it.blocks)}
            {:else}
              {@render itemBody(it.blocks, b.tight)}
            {/if}
          </li>
        {/each}
      </svelte:element>
    {/if}
  {/each}
{/snippet}

{#snippet itemBody(inner: Block[], tight: boolean)}
  <!-- 列表项的正文，**不能**跟上面那层复选框行一起合掉：
       - 有复选框时它是 flex 行的 item，靠 `flex-1` 占满剩下的宽度（实测：去掉后正文从
         354px 缩到 80px，工具行那种「图标 + 文字」的挤压就是这样来的）；
       - 没复选框时它负责**多块之间**的间距（`gap-*`）。只有一个段落时看不出区别，
         但一条列表项里有多个块很常见（「文字 + 嵌套列表」）。
       （这两句都是 `just dom-audit` 的拔掉测试实测的，不是推的。） -->
  <div class="flex min-w-0 flex-1 flex-col {tight ? 'gap-1' : 'gap-2.5'}">
    {@render blocks(inner)}
  </div>
{/snippet}

<!-- 块级内容分组。在 MessageList 里父级已经是 flex-col gap-2.5，看着像多余的一层，
     但思考面板（Sheet）里父级不是 flex 容器 —— 那里的段间距就靠这一层。
     只有一个块时这层没有段间距可管（拔掉测试 0 像素），就不套了。 -->
{#if doc.length > 1}
  <div class="flex flex-col gap-2.5">
    {@render blocks(doc)}
  </div>
{:else}
  <!-- 0 个块 = 什么都不画（别留一个空 div）；1 个块 = 直接放，省掉这层分组 -->
  {@render blocks(doc)}
{/if}
