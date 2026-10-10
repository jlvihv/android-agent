<script lang="ts">
  /**
   * 底部弹出面板（覆盖层）。一次只装**一个**东西 —— 一段思考，或一次工具调用。
   *
   * 尺寸按 Claude 的「摘要」面板实测（1080×2424 @420dpi → 411×923 css）：
   *   全宽贴底，上圆角 22，底色 #20201f（正好是已有的 --color-surface）
   *   遮罩：页面底色 #151515 被压到 #0e0e0e ≈ 35% 黑
   *   抓手 36×5 居中，距面板顶 5.3
   *   头部高 44，✕ 中心距左 38（= px-4 加 44 的命中区），标题 17px 半粗居中
   *   头部下面没有分隔线
   *
   * 高度用 max-h 而不是像 Claude 那样固定 422：内容短不留一大片空白，
   * 长就在面板内滚。面板是覆盖层，所以不影响正文布局。
   */
  import type { Snippet } from 'svelte';
  import Icon from './Icon.svelte';

  let {
    title,
    onclose,
    children,
  }: { title: string; onclose: () => void; children: Snippet } = $props();

  let panel: HTMLDivElement | null = $state(null);
  let body: HTMLDivElement | null = $state(null);

  /** 下拉位移。> 0 = 被手指往下拖了。 */
  let dy = $state(typeof window === 'undefined' ? 600 : window.innerHeight);
  /** 回弹动画开关。拖拽中必须关掉，否则跟手会卡。 */
  let animate = $state(false);
  let dragging = $state(false);
  let closing = $state(false);

  let startY = 0;
  let startAt = 0;
  let panelH = 1;

  // 进场：初始在屏幕外，下一帧滑进来
  $effect(() => {
    const t = setTimeout(() => {
      animate = true;
      dy = 0;
    }, 16);
    return () => clearTimeout(t);
  });

  // 拖到哪，遮罩淡到哪
  const dim = $derived(Math.max(0, 1 - dy / panelH));

  function close() {
    if (closing) return;
    closing = true;
    animate = true;
    panelH = panel?.offsetHeight ?? 400;
    dy = panelH;
    setTimeout(onclose, 180);
  }

  function down(e: PointerEvent) {
    if (closing) return;
    // 正文没滚到顶就不接管，让它自己滚
    if (body && body.scrollTop > 0) return;
    dragging = true;
    animate = false;
    startY = e.clientY;
    startAt = Date.now();
    panelH = panel?.offsetHeight ?? 400;
    panel?.setPointerCapture(e.pointerId);
  }

  function move(e: PointerEvent) {
    if (!dragging) return;
    dy = Math.max(0, e.clientY - startY);
  }

  function up() {
    if (!dragging) return;
    dragging = false;
    animate = true;
    const dt = Date.now() - startAt;
    // 甩一下（快 + 有位移）或者拖过 1/3 就关
    if ((dt < 250 && dy > 40) || dy > panelH / 3) close();
    else dy = 0;
  }
</script>

<!-- 遮罩。点它即关。 -->
<button
  class="fixed inset-0 z-40 bg-black/35"
  style:opacity={dim}
  aria-label="关闭"
  onclick={close}
></button>

<div
  bind:this={panel}
  role="dialog"
  aria-modal="true"
  aria-label={title}
  tabindex="-1"
  onpointerdown={down}
  onpointermove={move}
  onpointerup={up}
  onpointercancel={up}
  style:transform="translateY({dy}px)"
  class="fixed inset-x-0 bottom-0 z-50 flex max-h-[75vh] flex-col rounded-t-card bg-surface pt-1
         {animate ? 'transition-transform duration-200 ease-out' : ''}"
>
  <!-- 抓手 + 头部 = 下拉区域。touch-action:none 只加在这里，
       正文要保持原生滚动。 -->
  <div class="shrink-0 touch-none">
    <div class="mx-auto h-1.5 w-9 rounded-full bg-control"></div>

    <!-- ✕ 在左、标题居中。标题用 absolute 而不是 flex 居中 ——
         这样它永远在屏幕正中，不受 ✕ 宽度影响。 -->
    <div class="relative mt-3 flex h-11 items-center px-4">
      <button class="-m-2 p-2 text-fg" aria-label="关闭" onclick={close}>
        <Icon name="close" size={14} />
      </button>
      <h2
        class="pointer-events-none absolute inset-x-0 truncate px-12 text-center text-[17px] font-semibold"
      >
        {title}
      </h2>
    </div>
  </div>

  <div
    bind:this={body}
    class="min-h-0 flex-1 overflow-y-auto overscroll-contain px-4 pt-4 pb-[max(1rem,env(safe-area-inset-bottom))] selectable"
  >
    {@render children()}
  </div>
</div>
