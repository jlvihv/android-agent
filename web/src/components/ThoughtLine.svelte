<script lang="ts">
  /**
   * 思考摘要行：模型此刻在想什么。
   *
   * 流式时**只在换行时切换** —— 取最近一条写完的行，正在写的半行不显示。
   * 逐字刷新会让这一行一直抖，读不进去；按行切换配合亮起动画，就像一句
   * 一句冒出来。结束后停在最后一行（和流式时最后看到的一致，不跳）。
   *
   * 节奏：每行至少停留 HOLD 毫秒。快模型一秒能吐好几行（实测相邻两行
   * 只隔 120ms），每行都切的话亮起动画还没走完就被打断，看着是在闪。
   * 停留期间来的新行只记最新的一条，到点直接切过去，中间的跳过。
   */
  import { untrack } from 'svelte';

  let { text, live }: { text: string; live: boolean } = $props();

  const HOLD = 700;

  const latest = $derived(pick(text, live));
  // 历史消息首帧就是最终内容，直接显示、不播亮起动画
  let shown = $state(untrack(() => latest));
  /** 挂载之后真的换过行才播亮起动画 */
  let changed = $state(false);
  let since = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;

  $effect(() => {
    const next = latest;
    if (next === shown) return;
    clearTimeout(timer);
    const wait = since + HOLD - performance.now();
    const swap = () => {
      shown = latest;
      changed = true;
      since = performance.now();
    };
    if (wait <= 0) swap();
    else timer = setTimeout(swap, wait);
  });
  $effect(() => () => clearTimeout(timer));

  /** 从末尾往前找第一条非空行。流式时每个 delta 都会调到这里，
   *  思考可能有几十 KB，不能每次整段 split。 */
  function pick(t: string, streaming: boolean): string {
    let end = streaming ? t.lastIndexOf('\n') : t.length;
    while (end > 0) {
      const start = t.lastIndexOf('\n', end - 1) + 1;
      const l = t
        .slice(start, end)
        .replace(/^\s*(#+|[-*+]|\d+[.)])\s+/, '')
        .replace(/\*\*|__|`/g, '')
        .trim();
      if (l) return l;
      end = start - 1;
    }
    return '';
  }
</script>

{#key shown}
  <span class="thought min-w-0 truncate text-sm text-muted" class:live class:reveal={changed}
    >{shown || (live ? '思考中…' : '思考')}</span
  >
{/key}

<style>
  /*
   * 每换一行（{#key} 重建元素）从左到右亮起：遮罩是一条带软边的渐变，
   * 从右往左扫过去，字就一个接一个浮现，边缘是虚的所以不生硬。
   * 还在想的时候再叠一层流光（亮带循环扫过文字）。
   * 只有这一行在动，重绘面积很小。
   */
  .reveal {
    -webkit-mask-image: linear-gradient(90deg, #000 0 33%, transparent 66% 100%);
    mask-image: linear-gradient(90deg, #000 0 33%, transparent 66% 100%);
    -webkit-mask-size: 300% 100%;
    mask-size: 300% 100%;
    -webkit-mask-repeat: no-repeat;
    mask-repeat: no-repeat;
    animation: thought-reveal 650ms cubic-bezier(0.25, 0.6, 0.3, 1) both;
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
      thought-reveal 650ms cubic-bezier(0.25, 0.6, 0.3, 1) both,
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
