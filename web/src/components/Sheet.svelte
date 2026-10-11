<script lang="ts">
  /**
   * 底部弹出面板（覆盖层）。一次只装**一个**东西 —— 一段思考，或一次工具调用。
   *
   * 尺寸按 Claude 的「摘要」面板实测（1080×2424 @420dpi → 411×923 css）：
   *   全宽贴底，上圆角 22，底色 #20201f（正好是已有的 --color-surface）
   *   遮罩：页面底色 #151515 被压到 #0e0e0e ≈ 35% 黑
   *   抓手 36×5 居中，距面板顶 5.3
   *   标题 17px 半粗居中，头部下面没有分隔线
   *
   * 没有关闭按钮：往下拉、点遮罩、按返回键都能关。
   *
   * 高度：最少 45%（Claude 固定 422/923 ≈ 46%），内容短也不会缩成一条；
   * 最多 75%，再长就在面板内滚。
   */
  import type { Snippet } from 'svelte';
  import { onBack } from '../lib/overlay';
  import { dismissable, RELEASE } from '../lib/drag';

  let {
    title,
    onclose,
    children,
  }: { title: string; onclose: () => void; children: Snippet } = $props();

  let panel: HTMLDivElement | null = $state(null);
  let body: HTMLDivElement | null = $state(null);

  /** 0 = 完全展开，1 = 完全收起（在屏幕外） */
  let p = $state(1);
  /** 跟手中：不能有过渡，否则手指和面板之间有延迟 */
  let dragging = $state(false);
  let closing = false;

  // 进场：初始在屏幕外，等首帧画出来再滑进来。
  // 两层 rAF：第一层回调时初始位置还没提交，直接改会被合并成一帧、没有动画。
  $effect(() => {
    let r = requestAnimationFrame(() => {
      r = requestAnimationFrame(() => (p = 0));
    });
    return () => cancelAnimationFrame(r);
  });

  // 返回键关面板（带动画）
  $effect(() => onBack(close));

  /**
   * 挂到 body 上。
   *
   * 思考/工具面板是在消息列表里渲染的，而消息列表在 `<main class="z-0">`
   * 里 —— 那是个独立的层叠上下文，里面的 z-50 再高也压不过外面 z-20 的
   * 输入卡片，结果输入框叠在面板上面。搬到 body 下就和输入卡片平级比了。
   *
   * 遮罩和面板各挂各的（不再用一个 `display:contents` 的包装元素）：
   * 那个包装只为当 portal 的落点，真正干活的就这两个节点。
   */
  function portal(node: HTMLElement) {
    document.body.appendChild(node);
    return { destroy: () => node.remove() };
  }

  /** 外部想关（比如保存成功）也走动画 */
  export function dismiss() {
    close();
  }

  function close() {
    if (closing) return;
    closing = true;
    dragging = false;
    p = 1;
    setTimeout(onclose, 280);
  }

  const drag = {
    axis: 'y' as const,
    dir: 1 as const,
    size: () => panel?.offsetHeight ?? 400,
    // 正文没滚到顶时往下拉是在滚正文，不接管
    canStart: (t: EventTarget | null) =>
      !closing && !(body && body.contains(t as Node) && body.scrollTop > 0),
    onStart: () => (dragging = true),
    onMove: (v: number) => (p = v),
    onEnd: (shouldClose: boolean) => {
      dragging = false;
      if (shouldClose) close();
      else p = 0;
    },
  };
</script>

<!-- 遮罩。点它即关；拖到哪淡到哪。 -->
<button
  use:portal
  class="fixed inset-0 z-40 bg-black/35"
  style:opacity={1 - p}
  style:transition={dragging ? 'none' : 'opacity 280ms ease-out'}
  aria-label="关闭"
  onclick={close}
></button>

<div
  use:portal
  bind:this={panel}
  use:dismissable={drag}
  role="dialog"
  aria-modal="true"
  aria-label={title}
  tabindex="-1"
  style:transform="translateY({p * 100}%)"
  style:transition={dragging ? 'none' : RELEASE}
  class="fixed inset-x-0 bottom-[var(--kb,0px)] z-50 flex max-h-[75dvh] min-h-[45dvh] flex-col
         rounded-t-card bg-surface pt-1 will-change-transform"
>
  <div class="shrink-0">
    <div class="mx-auto h-1.5 w-9 rounded-full bg-control"></div>
    <h2 class="mt-3 flex h-11 items-center justify-center truncate px-12 text-[17px] font-semibold">
      {title}
    </h2>
  </div>

  <div
    bind:this={body}
    class="min-h-0 flex-1 overflow-y-auto overscroll-contain px-4 pt-4 pb-[max(1rem,env(safe-area-inset-bottom))] selectable"
  >
    {@render children()}
  </div>
</div>
