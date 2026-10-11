<script lang="ts">
  /**
   * 沙箱里的文本文件（.sh / .py / .json / .toml / 无后缀的…）。
   *
   * 为什么不直接丢给 iframe：服务端对这些类型发的是 `text/plain`（浏览器能显示），
   * 但那是白底、无样式、整段不可复制的原始视图。自己拉出来渲染：深色、等宽、
   * 能复制，顺手把行数/体积摆出来。
   */
  let { url }: { url: string } = $props();

  /** 超过这个大小只显示开头 —— 手机内存有限，别为看一个 log 把 WebView 撑爆 */
  const LIMIT = 200 * 1024;

  let text = $state<string | null>(null);
  let note = $state('');
  let err = $state('');
  let copied = $state(false);

  $effect(() => {
    // 必须**同步**读一下 url，否则依赖收集不到（fetch 在 async 里读就不算数了）
    void url;
    let dead = false;
    text = null;
    note = '';
    err = '';
    void (async () => {
      try {
        const r = await fetch(url);
        if (!r.ok) {
          // 服务端对这些错误页给的是 HTML（给查看器里的用户看的），
          // 这里抠出文字来，比只显示 “HTTP 404” 有用得多
          const body = await r.text();
          const plain = body
            .replace(/<[^>]*>/g, ' ')
            .replace(/\s+/g, ' ')
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

  const lines = $derived(text == null ? 0 : text.split('\n').length);

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
  <div class="flex shrink-0 items-center gap-2 px-3 py-1.5 font-mono text-meta text-faint">
    {#if text != null}
      <span>{lines} 行 · {(text.length / 1024).toFixed(1)}KB</span>
      {#if note}<span class="truncate text-warn">{note}</span>{/if}
      <button class="ml-auto shrink-0 rounded-full bg-control px-3 py-1 text-fg" onclick={copy}>
        {copied ? '已复制' : '复制'}
      </button>
    {/if}
  </div>

  <div class="min-h-0 flex-1 overflow-auto px-3 pb-[max(0.75rem,env(safe-area-inset-bottom))]">
    {#if err}
      <p class="py-6 text-sm text-bad">读不出来：{err}</p>
    {:else if text == null}
      <p class="py-6 text-sm text-faint">读取中…</p>
    {:else}
      <pre
        class="font-mono text-[13px] leading-5 whitespace-pre-wrap break-words text-fg">{text}</pre>
    {/if}
  </div>
</div>
