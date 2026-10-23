<script lang="ts">
  /**
   * 查看器里的 Markdown 文件（`.md` / `.markdown` / `.mkd`）。
   *
   * 和 TextView 一样**自己把文件拉出来**（iframe 里是白底无样式的原始视图），
   * 但交给 Markdown.svelte 渲染 —— 消息里怎么显示，看文档就怎么显示。拉之前
   * 看到的还是 TextView 那套（行数 / 体积 / 复制）。
   *
   * 关键是 `base`：md 里的相对链接（`[docs/07](07-debugging.md)`、
   * `![](images/a.png)`）是**相对文件自己**写的，得按文件所在目录补成沙箱里的
   * 绝对路径才渲染得出来（见 lib/markdown.ts 的 resolveRelative）。
   */
  import Markdown from "./Markdown.svelte";
  import { sandboxGuestPath } from "../lib/markdown";

  let { url }: { url: string } = $props();

  /** 超过这个大小只显示开头 —— 和 TextView 一个尺度，别为看一篇文档把 WebView 撑爆 */
  const LIMIT = 200 * 1024;

  /** 文件所在目录（沙箱内绝对路径，以 / 结尾）。取不到（非沙箱地址）就当消息那样渲染 */
  const dir = $derived.by(() => {
    const p = sandboxGuestPath(url);
    return p ? p.replace(/[^/]*$/, "") : "";
  });

  let text = $state<string | null>(null);
  let note = $state("");
  let err = $state("");
  let copied = $state(false);

  $effect(() => {
    // 必须**同步**读一下 url，否则依赖收集不到（fetch 在 async 里读就不算数了）
    void url;
    let dead = false;
    text = null;
    note = "";
    err = "";
    void (async () => {
      try {
        const r = await fetch(url);
        if (!r.ok) {
          // 服务端对这些错误页给的是 HTML（给查看器里的用户看的），
          // 这里抠出文字来，比只显示 “HTTP 404” 有用得多
          const body = await r.text();
          const plain = body
            .replace(/<[^>]*>/g, " ")
            .replace(/\s+/g, " ")
            .trim();
          throw new Error(plain || `HTTP ${r.status}`);
        }
        const body = await r.text();
        if (dead) return;
        if (body.length > LIMIT) {
          text = body.slice(0, LIMIT);
          note = `只显示前 ${LIMIT / 1024}KB（共 ${(body.length / 1024).toFixed(0)}KB）`;
        } else {
          text = body;
        }
      } catch (e) {
        if (!dead) err = String(e);
      }
    })();
    return () => {
      dead = true;
    };
  });

  async function copy() {
    if (text == null) return;
    try {
      await navigator.clipboard.writeText(text);
      copied = true;
      setTimeout(() => (copied = false), 1500);
    } catch {
      /* 剪贴板被拒就算了 */
    }
  }
</script>

<div class="flex h-full flex-col">
  <div
    class="flex shrink-0 items-center gap-2 px-3 py-1.5 font-mono text-meta text-faint"
  >
    {#if text != null}
      <span>{(text.length / 1024).toFixed(1)}KB</span>
      {#if note}<span class="truncate text-warn">{note}</span>{/if}
      <button
        class="ml-auto shrink-0 rounded-full bg-control px-3 py-1 text-fg"
        onclick={copy}
      >
        {copied ? "已复制" : "复制"}
      </button>
    {/if}
  </div>

  <div
    class="min-h-0 flex-1 overflow-auto px-3 pb-[max(0.75rem,env(safe-area-inset-bottom))]"
  >
    {#if err}
      <p class="py-6 text-sm text-bad">读不出来：{err}</p>
    {:else if text == null}
      <p class="py-6 text-sm text-faint">读取中…</p>
    {:else}
      <!-- 顶栏已经占了一行，正文自己的上边距收掉一点，别一眼看着是空的 -->
      <div class="-mt-1 text-fg"><Markdown {text} base={dir} /></div>
    {/if}
  </div>
</div>
